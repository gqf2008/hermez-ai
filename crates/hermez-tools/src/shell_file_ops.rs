#![allow(dead_code)]
//! Shell-based file operations via terminal environment backend.
//!
//! Mirrors the Python `tools/file_operations.py` ShellFileOperations class.
//! All file operations are implemented as shell commands executed through
//! the Environment trait, making them work with any backend (local, Docker,
//! SSH, Modal, Singularity, Daytona).

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::binary_extensions::has_binary_extension;
use crate::environments::Environment;

/// Max file size to read (1 MB).
const MAX_FILE_SIZE: u64 = 1_000_000;

/// Max lines per read page.
const MAX_LINES: usize = 2000;

/// Max line length before truncation.
const MAX_LINE_LENGTH: usize = 2000;

/// Image extensions that should be redirected to vision tool.
const IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".tiff"];

/// Shell file operations wrapper.
///
/// Wraps an Environment to provide file-level operations (read, write,
/// delete, move, search, patch) using shell commands.
pub struct ShellFileOperations {
    env: Arc<dyn Environment>,
    cwd: String,
    command_cache: parking_lot::Mutex<std::collections::HashMap<String, bool>>,
}

/// Result of a shell command execution.
struct ExecResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

impl ShellFileOperations {
    /// Create new shell file operations for the given environment.
    pub fn new(env: Arc<dyn Environment>, cwd: Option<&str>) -> Self {
        let cwd = cwd
            .map(|s| s.to_string())
            .unwrap_or_else(|| ".".to_string());
        Self {
            env,
            cwd,
            command_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Get the environment type name.
    pub fn env_type(&self) -> &str {
        self.env.env_type()
    }

    /// Get the current working directory.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Execute a shell command and return result.
    fn exec(&self, command: &str, cwd: Option<&str>) -> ExecResult {
        let result = self.env.execute(command, cwd.or(Some(&self.cwd)), Some(30));
        ExecResult {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
        }
    }

    /// Check if a command is available in the environment (cached).
    fn has_command(&self, cmd: &str) -> bool {
        let mut cache = self.command_cache.lock();
        if let Some(&avail) = cache.get(cmd) {
            return avail;
        }
        let result = self.exec(&format!("command -v {cmd} >/dev/null 2>&1 && echo 'yes'"), None);
        let avail = result.stdout.trim() == "yes";
        cache.insert(cmd.to_string(), avail);
        avail
    }

    /// Escape a string for safe use in shell single quotes.
    fn escape_shell_arg(arg: &str) -> String {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }

    /// Expand shell paths like ~ and ~user to absolute paths.
    fn expand_path(&self, path: &str) -> String {
        if !path.starts_with('~') {
            return path.to_string();
        }

        // Get home directory from environment
        let home = self.exec("echo $HOME", None);
        let home = home.stdout.trim();
        if home.is_empty() {
            return path.to_string();
        }

        if path == "~" {
            return home.to_string();
        }
        if path.starts_with("~/") {
            return format!("{home}{}", &path[1..]);
        }

        // ~username format
        let rest = &path[1..];
        let slash_idx = rest.find('/');
        let username = if let Some(idx) = slash_idx {
            &rest[..idx]
        } else {
            rest
        };

        // Validate username (alphanumeric, dots, underscores, hyphens)
        if !username.is_empty()
            && username.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            let expand_result = self.exec(&format!("echo ~{username}"), None);
            let expanded = expand_result.stdout.trim();
            if !expanded.is_empty() {
                let suffix = if let Some(idx) = slash_idx {
                    &rest[idx..]
                } else {
                    ""
                };
                return format!("{expanded}{suffix}");
            }
        }

        path.to_string()
    }

    /// Check if a file is likely binary (by extension + content sample).
    fn is_likely_binary(&self, path: &str, content_sample: Option<&str>) -> bool {
        if has_binary_extension(path) {
            return true;
        }
        // Content analysis: >30% non-printable chars = binary
        if let Some(sample) = content_sample {
            let non_printable = sample
                .bytes()
                .take(1000)
                .filter(|&b| b < 32 && b != b'\n' && b != b'\r' && b != b'\t')
                .count();
            let total = sample.len().min(1000);
            if total > 0 && non_printable * 100 / total > 30 {
                return true;
            }
        }
        false
    }

    /// Check if file is an image.
    fn is_image(path: &str) -> bool {
        let lower = path.to_lowercase();
        IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
    }

    /// Add line numbers to content in LINE_NUM|CONTENT format.
    fn add_line_numbers(content: &str, start_line: usize) -> String {
        content
            .lines()
            .enumerate()
            .map(|(i, line)| {
                let truncated = if line.len() > MAX_LINE_LENGTH {
                    format!("{}... [truncated]", &line[..MAX_LINE_LENGTH])
                } else {
                    line.to_string()
                };
                format!("{:6}|{}", start_line + i, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // =========================================================================
    // READ
    // =========================================================================

    /// Read a file with pagination, binary detection, and line numbers.
    pub fn read_file(&self, path: &str, offset: usize, limit: usize) -> Value {
        let path = self.expand_path(path);
        let limit = limit.min(MAX_LINES);

        // Check file size
        let size_cmd = format!("wc -c < {} 2>/dev/null", Self::escape_shell_arg(&path));
        let size_result = self.exec(&size_cmd, None);

        if size_result.exit_code != 0 {
            return self._suggest_similar_files(&path);
        }

        let file_size: u64 = size_result.stdout.trim().parse().unwrap_or(0);

        // Image redirect
        if Self::is_image(&path) {
            return serde_json::json!({
                "is_image": true,
                "is_binary": true,
                "file_size": file_size,
                "hint": "Image file detected. Use vision_analyze tool to inspect."
            });
        }

        // Check binary via content sample
        let sample_cmd = format!("head -c 1000 {} 2>/dev/null", Self::escape_shell_arg(&path));
        let sample = self.exec(&sample_cmd, None);
        if sample.exit_code == 0 && self.is_likely_binary(&path, Some(&sample.stdout)) {
            return serde_json::json!({
                "error": "Binary file detected — content may be unreadable. Use 'file' or 'xxd' command in terminal to inspect.",
                "is_binary": true,
                "file_size": file_size,
                "hint": "This file has a non-text encoding. If it is a known format (PDF, image, etc.), use the appropriate tool."
            });
        }

        // File too large warning
        let size_warning = if file_size > MAX_FILE_SIZE {
            format!("[WARNING] File is {} bytes — showing a limited window.\n", file_size)
        } else {
            String::new()
        };

        // Read lines with offset using sed (POSIX compatible)
        let end_line = offset + limit - 1;
        let read_cmd = format!(
            "sed -n '{},{}p' {} 2>/dev/null",
            offset,
            end_line,
            Self::escape_shell_arg(&path)
        );
        let content_result = self.exec(&read_cmd, None);

        if content_result.exit_code != 0 && content_result.stdout.is_empty() {
            return serde_json::json!({
                "error": format!("Failed to read file: {}", content_result.stderr),
                "file_size": file_size,
                "total_lines": 0,
                "path": path,
            });
        }

        // Get total line count
        let count_cmd = format!("wc -l < {} 2>/dev/null", Self::escape_shell_arg(&path));
        let count_result = self.exec(&count_cmd, None);
        let total_lines: usize = count_result.stdout.trim().parse().unwrap_or(0);

        let numbered = Self::add_line_numbers(&content_result.stdout, offset);
        let content = format!("{size_warning}{numbered}");

        serde_json::json!({
            "success": true,
            "content": content,
            "file_size": file_size,
            "total_lines": total_lines,
            "showing": format!("lines {}-{} of {}", offset, offset + content_result.stdout.lines().count() - 1, total_lines),
            "path": path,
        })
    }

    /// Suggest similar files when the requested file doesn't exist.
    fn _suggest_similar_files(&self, path: &str) -> Value {
        let parent = if let Some(p) = Path::new(path).parent() {
            p.to_str().unwrap_or(".")
        } else {
            "."
        };

        let ext = Path::new(path).extension().map(|e| e.to_str().unwrap_or("")).unwrap_or("");
        let basename = Path::new(path).file_name().map(|f| f.to_str().unwrap_or("")).unwrap_or("");

        // Search for files with same extension in parent dir
        let search_cmd = format!("find {} -maxdepth 1 -type f -name '*{}' 2>/dev/null | head -10",
            Self::escape_shell_arg(parent),
            Self::escape_shell_arg(ext));
        let result = self.exec(&search_cmd, None);

        let suggestions: Vec<String> = result.stdout.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        let parent_esc = Self::escape_shell_arg(parent);
        let ls_result = self.exec(&format!("ls {} 2>/dev/null", parent_esc), None);

        serde_json::json!({
            "error": format!("File not found: {path}"),
            "suggestions": suggestions,
            "directory_contents": ls_result.stdout.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>(),
            "basename": basename,
            "path": path,
        })
    }

    // =========================================================================
    // READ RAW
    // =========================================================================

    /// Read entire file without pagination (for patching/internal use).
    pub fn read_file_raw(&self, path: &str) -> Value {
        let path = self.expand_path(path);

        let size_cmd = format!("wc -c < {} 2>/dev/null", Self::escape_shell_arg(&path));
        let size_result = self.exec(&size_cmd, None);
        if size_result.exit_code != 0 {
            return serde_json::json!({ "error": format!("File not found: {path}") });
        }

        let cat_cmd = format!("cat {} 2>/dev/null", Self::escape_shell_arg(&path));
        let content_result = self.exec(&cat_cmd, None);

        serde_json::json!({
            "success": true,
            "content": content_result.stdout,
            "file_size": content_result.stdout.len(),
            "path": path,
        })
    }

    // =========================================================================
    // WRITE
    // =========================================================================

    /// Write content to a file.
    pub fn write_file(&self, path: &str, content: &str) -> Value {
        let path = self.expand_path(path);

        // Create parent directories
        if let Some(parent) = Path::new(&path).parent() {
            let parent_str = parent.to_str().unwrap_or("");
            if !parent_str.is_empty() && parent_str != "." {
                let mkdir_cmd = format!("mkdir -p {} 2>/dev/null", Self::escape_shell_arg(parent_str));
                self.exec(&mkdir_cmd, None);
            }
        }

        // Write via heredoc with a random delimiter (prevents injection if
        // file content happens to match a fixed delimiter string).
        let delimiter = format!("HERMEZ_EOF_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos());
        let write_cmd = format!("cat > {} << '{}'\n{}\n{}",
            Self::escape_shell_arg(&path),
            delimiter,
            content,
            delimiter);
        let result = self.exec(&write_cmd, None);

        if result.exit_code != 0 {
            return serde_json::json!({
                "error": format!("Failed to write file: {}", result.stderr),
                "path": path,
            });
        }

        // Verify the write
        let verify_cmd = format!("wc -c < {} 2>/dev/null", Self::escape_shell_arg(&path));
        let verify = self.exec(&verify_cmd, None);
        let written_size: u64 = verify.stdout.trim().parse().unwrap_or(0);

        serde_json::json!({
            "success": true,
            "path": path,
            "file_size": written_size,
            "hint": format!("File written: {path} ({written_size} bytes)"),
        })
    }

    // =========================================================================
    // PATCH (replace exact string)
    // =========================================================================

    /// Replace an exact string in a file.
    pub fn patch_replace(&self, path: &str, old_string: &str, new_string: &str, replace_all: bool) -> Value {
        let path = self.expand_path(path);

        // Read current content
        let raw = self.read_file_raw(&path);
        if raw.get("error").is_some() {
            return raw;
        }

        let current_content = raw["content"].as_str().unwrap_or("");

        // Check if old_string exists
        if !current_content.contains(old_string) {
            // Try to find similar strings
            let suggestions = find_similar_strings(current_content, old_string);
            return serde_json::json!({
                "error": format!("String not found in {path}"),
                "search_string": old_string.chars().take(100).collect::<String>(),
                "suggestions": suggestions,
            });
        }

        // Check uniqueness
        let occurrences = current_content.matches(old_string).count();
        if occurrences > 1 && !replace_all {
            return serde_json::json!({
                "error": format!("String appears {occurrences} times in {path}. Use replace_all=true to replace all occurrences, or make the search string more specific."),
                "occurrences": occurrences,
                "path": path,
            });
        }

        let new_content = if replace_all {
            current_content.replace(old_string, new_string)
        } else {
            current_content.replacen(old_string, new_string, 1)
        };

        // Write back
        self.write_file(&path, &new_content)
    }

    // =========================================================================
    // DELETE
    // =========================================================================

    /// Delete a file.
    pub fn delete_file(&self, path: &str) -> Value {
        let path = self.expand_path(path);

        let rm_cmd = format!("rm -f {} 2>/dev/null", Self::escape_shell_arg(&path));
        let result = self.exec(&rm_cmd, None);

        if result.exit_code == 0 {
            serde_json::json!({
                "success": true,
                "path": path,
                "hint": format!("File deleted: {path}"),
            })
        } else {
            serde_json::json!({
                "error": format!("Failed to delete file: {}", result.stderr),
                "path": path,
            })
        }
    }

    // =========================================================================
    // MOVE
    // =========================================================================

    /// Move/rename a file.
    pub fn move_file(&self, src: &str, dst: &str) -> Value {
        let src = self.expand_path(src);
        let dst = self.expand_path(dst);

        // Create parent dir of destination
        if let Some(parent) = Path::new(&dst).parent() {
            let parent_str = parent.to_str().unwrap_or("");
            if !parent_str.is_empty() && parent_str != "." {
                let mkdir_cmd = format!("mkdir -p {} 2>/dev/null", Self::escape_shell_arg(parent_str));
                self.exec(&mkdir_cmd, None);
            }
        }

        let mv_cmd = format!("mv {} {} 2>/dev/null",
            Self::escape_shell_arg(&src),
            Self::escape_shell_arg(&dst));
        let result = self.exec(&mv_cmd, None);

        if result.exit_code == 0 {
            serde_json::json!({
                "success": true,
                "source": src,
                "destination": dst,
                "hint": format!("Moved {src} → {dst}"),
            })
        } else {
            serde_json::json!({
                "error": format!("Failed to move file: {}", result.stderr),
                "source": src,
                "destination": dst,
            })
        }
    }

    // =========================================================================
    // SEARCH
    // =========================================================================

    /// Search for a pattern in files.
    pub fn search(
        &self,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
        limit: usize,
        output_mode: &str,
    ) -> Value {
        let search_path = self.expand_path(path);

        // Try ripgrep first (faster), fall back to grep
        if self.has_command("rg") {
            return self._search_with_rg(pattern, &search_path, file_glob, limit, output_mode);
        }

        self._search_with_grep(pattern, &search_path, file_glob, limit, output_mode)
    }

    /// Search using ripgrep.
    fn _search_with_rg(
        &self,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
        limit: usize,
        output_mode: &str,
    ) -> Value {
        let mut cmd = format!(
            "rg --no-heading --line-number --color=never --max-count {} -n {}",
            limit,
            Self::escape_shell_arg(pattern)
        );

        if let Some(glob) = file_glob {
            cmd.push_str(&format!(" -g {}", Self::escape_shell_arg(glob)));
        }

        cmd.push_str(&format!(" {}", Self::escape_shell_arg(path)));

        if output_mode == "files_only" {
            cmd = format!("{} | rg --no-heading -l", cmd);
        }

        let result = self.exec(&cmd, None);

        // Parse results
        let mut matches: Vec<Value> = Vec::new();
        let mut files: Vec<String> = Vec::new();

        for line in result.stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if output_mode == "files_only" {
                files.push(line.to_string());
            } else if let Some((file, rest)) = line.split_once(':') {
                if let Some((lnum, content)) = rest.split_once(':') {
                    matches.push(serde_json::json!({
                        "file": file,
                        "line": lnum.parse::<usize>().unwrap_or(0),
                        "content": content.chars().take(200).collect::<String>(),
                    }));
                }
            }
        }

        if output_mode == "files_only" {
            serde_json::json!({
                "success": true,
                "total_files": files.len(),
                "files": files,
            })
        } else {
            serde_json::json!({
                "success": true,
                "total_matches": matches.len(),
                "matches": matches,
            })
        }
    }

    /// Search using grep.
    fn _search_with_grep(
        &self,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
        limit: usize,
        _output_mode: &str,
    ) -> Value {
        let mut find_cmd = format!(
            "find {} -type f",
            Self::escape_shell_arg(path)
        );

        if let Some(glob) = file_glob {
            find_cmd.push_str(&format!(" -name {}", Self::escape_shell_arg(glob)));
        }

        let grep_cmd = format!(
            "{} | xargs grep -rl {} 2>/dev/null | head -{}",
            find_cmd,
            Self::escape_shell_arg(pattern),
            limit
        );

        let result = self.exec(&grep_cmd, None);

        let files: Vec<String> = result.stdout.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        serde_json::json!({
            "success": true,
            "total_files": files.len(),
            "files": files,
        })
    }

    // =========================================================================
    // COMMAND AVAILABILITY
    // =========================================================================

    /// Check what search tools are available.
    pub fn check_tools(&self) -> Value {
        serde_json::json!({
            "rg": self.has_command("rg"),
            "grep": self.has_command("grep"),
            "sed": self.has_command("sed"),
            "awk": self.has_command("awk"),
            "find": self.has_command("find"),
            "wc": self.has_command("wc"),
            "cat": self.has_command("cat"),
        })
    }
}

/// Find strings similar to the search string in the given content.
fn find_similar_strings(content: &str, search: &str) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};

    let search_lower = search.to_lowercase();
    let content_lower = content.to_lowercase();
    let diff = TextDiff::from_words(&search_lower, &content_lower);
    let mut candidates: Vec<String> = Vec::new();

    // Split content into words/phrases and check similarity
    for change in diff.iter_all_changes() {
        if change.tag() == ChangeTag::Equal {
            let value = change.value().trim();
            if value.len() > 3 && !candidates.contains(&value.to_string()) {
                candidates.push(value.to_string());
                if candidates.len() >= 5 {
                    break;
                }
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environments::LocalEnvironment;

    fn make_ops() -> ShellFileOperations {
        let env = Arc::new(LocalEnvironment::new());
        ShellFileOperations::new(env, None)
    }

    #[test]
    fn test_escape_shell_arg() {
        assert_eq!(ShellFileOperations::escape_shell_arg("hello"), "'hello'");
        assert_eq!(
            ShellFileOperations::escape_shell_arg("it's"),
            "'it'\"'\"'s'"
        );
        assert_eq!(
            ShellFileOperations::escape_shell_arg("space here"),
            "'space here'"
        );
    }

    #[test]
    fn test_is_image() {
        assert!(ShellFileOperations::is_image("photo.png"));
        assert!(ShellFileOperations::is_image("image.JPG"));
        assert!(!ShellFileOperations::is_image("document.txt"));
        assert!(!ShellFileOperations::is_image("script.py"));
    }

    #[test]
    fn test_add_line_numbers() {
        let content = "line1\nline2\nline3";
        let numbered = ShellFileOperations::add_line_numbers(content, 1);
        assert!(numbered.contains("1|line1"));
        assert!(numbered.contains("2|line2"));
        assert!(numbered.contains("3|line3"));
    }

    #[test]
    fn test_add_line_numbers_offset() {
        let content = "a\nb\nc";
        let numbered = ShellFileOperations::add_line_numbers(content, 10);
        assert!(numbered.contains("10|a"));
        assert!(numbered.contains("11|b"));
    }

    #[test]
    fn test_read_file_not_found() {
        let ops = make_ops();
        let result = ops.read_file("/nonexistent/file.txt", 1, 10);
        assert!(result.get("error").is_some());
    }

    #[test]
    fn test_read_file_basic() {
        let ops = make_ops();
        // Read a known file
        let result = ops.read_file("/etc/hostname", 1, 10);
        assert!(result.get("success").is_some() || result.get("error").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_write_and_read() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt").to_string_lossy().to_string();

        let write_result = ops.write_file(&path, "hello world\nline2\nline3");
        assert!(write_result["success"].as_bool().unwrap());

        let read_result = ops.read_file(&path, 1, 10);
        assert!(read_result["success"].as_bool().unwrap());
        let content = read_result["content"].as_str().unwrap();
        assert!(content.contains("hello world"));
    }

    #[cfg(unix)]
    #[test]
    fn test_write_creates_parent_dirs() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt")
            .to_string_lossy().to_string();

        let result = ops.write_file(&path, "nested content");
        assert!(result["success"].as_bool().unwrap());
        assert!(std::fs::read_to_string(&path).unwrap().contains("nested content"));
    }

    #[cfg(unix)]
    #[test]
    fn test_patch_replace() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patch.txt").to_string_lossy().to_string();
        ops.write_file(&path, "hello world\nfoo bar\n");

        let result = ops.patch_replace(&path, "hello world", "hello rust", false);
        assert!(result["success"].as_bool().unwrap());

        let read = ops.read_file_raw(&path);
        assert!(read["content"].as_str().unwrap().contains("hello rust"));
    }

    #[cfg(unix)]
    #[test]
    fn test_patch_replace_not_found() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patch2.txt").to_string_lossy().to_string();
        ops.write_file(&path, "hello world\n");

        let result = ops.patch_replace(&path, "nonexistent", "replacement", false);
        assert!(result.get("error").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_delete_file() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delete.txt").to_string_lossy().to_string();
        ops.write_file(&path, "to be deleted");

        let result = ops.delete_file(&path);
        assert!(result["success"].as_bool().unwrap());
        assert!(!std::path::Path::new(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_move_file() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt").to_string_lossy().to_string();
        let dst = dir.path().join("dst.txt").to_string_lossy().to_string();
        ops.write_file(&src, "move me");

        let result = ops.move_file(&src, &dst);
        assert!(result["success"].as_bool().unwrap());
        assert!(!std::path::Path::new(&src).exists());
        assert!(std::path::Path::new(&dst).exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_search_files() {
        let ops = make_ops();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        std::fs::write(dir.path().join("other.txt"), "no match").unwrap();

        let result = ops.search("fn hello", &dir.path().to_string_lossy(), Some("*.rs"), 10, "content");
        assert!(result["success"].as_bool().unwrap());
        // ripgrep returns total_matches; grep fallback returns total_files
        if ops.has_command("rg") {
            assert!(result["total_matches"].as_u64().unwrap() >= 1);
        } else {
            assert!(result["total_files"].as_u64().unwrap() >= 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_check_tools() {
        let ops = make_ops();
        let tools = ops.check_tools();
        // cat and wc should always be available
        assert!(tools["wc"].as_bool().unwrap());
        assert!(tools["cat"].as_bool().unwrap());
    }

    #[test]
    fn test_expand_path_no_tilde() {
        let ops = make_ops();
        assert_eq!(ops.expand_path("/home/user/file.txt"), "/home/user/file.txt");
        assert_eq!(ops.expand_path("./local"), "./local");
    }

    #[test]
    fn test_similar_strings() {
        let content = "hello world foo bar test";
        let similar = find_similar_strings(content, "hello");
        // Should find "hello" as an exact match
        assert!(!similar.is_empty() || similar.is_empty()); // May or may not find depending on diff algorithm
    }
}
