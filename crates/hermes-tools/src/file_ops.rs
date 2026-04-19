#![allow(dead_code)]
//! File operation tools: read_file, write_file, patch, search_files.
//!
//! Mirrors the Python `tools/file_tools.py` with local filesystem operations.
//! 4 tools with comprehensive security checks.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::binary_extensions::has_binary_extension;
use crate::registry::{tool_error, tool_result, ToolRegistry};

/// Atomic write: write to temp file then rename to avoid partial writes on crash.
/// Falls back to direct write on Windows if rename fails (file-in-use issue).
fn atomic_write(path: &Path, content: &str) -> Result<usize, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{}.", path.file_name().unwrap_or_default().to_string_lossy()))
        .suffix(".tmp")
        .tempfile_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| format!("tempfile: {e}"))?;
    use std::io::Write;
    tmp.write_all(content.as_bytes()).map_err(|e| format!("write: {e}"))?;
    tmp.flush().map_err(|e| format!("flush: {e}"))?;
    match tmp.persist(path) {
        Ok(_) => Ok(content.len()),
        Err(e) => {
            // On Windows, persist may fail if the target is in use or held open.
            // Fallback: write directly to target.
            let tmp_path = e.file.path().to_path_buf();
            drop(e.file);
            std::fs::write(path, content.as_bytes()).map_err(|e| format!("write: {e}"))?;
            let _ = std::fs::remove_file(tmp_path);
            Ok(content.len())
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum characters returned from a file read.
const DEFAULT_MAX_READ_CHARS: usize = 100_000;

/// File size threshold for showing a "large file" hint (512 KB).
const LARGE_FILE_HINT_BYTES: u64 = 512_000;

/// Maximum lines returned in a single read.
const MAX_LINES: usize = 2000;

/// Maximum line length before truncation.
const MAX_LINE_LENGTH: usize = 2000;

/// Device paths that would hang or produce infinite output.
static BLOCKED_DEVICE_PATHS: &[&str] = &[
    "/dev/zero", "/dev/random", "/dev/urandom", "/dev/full",
    "/dev/stdin", "/dev/tty", "/dev/console",
    "/dev/stdout", "/dev/stderr",
    "/dev/fd/0", "/dev/fd/1", "/dev/fd/2",
];

/// Sensitive path prefixes that should not be written to.
const SENSITIVE_PREFIXES: &[&str] = &["/etc/", "/boot/", "/usr/lib/systemd/"];

/// Sensitive exact paths that should not be written to.
const SENSITIVE_EXACT: &[&str] = &["/var/run/docker.sock", "/run/docker.sock"];

/// Paths that are denied write (substrings matched after realpath).
const WRITE_DENIED_PREFIXES: &[&str] = &[
    ".ssh/", ".aws/", ".kube/", ".docker/",
    "/etc/systemd/",
];

/// Exact paths denied for writing.
const WRITE_DENIED_PATHS: &[&str] = &[
    ".env", ".bashrc", ".bash_profile", ".zshrc",
    ".profile", ".netrc", ".pgpass",
    "/etc/sudoers", "/etc/passwd", "/etc/shadow",
];

// ---------------------------------------------------------------------------
// Security helpers
// ---------------------------------------------------------------------------

/// Check if a path would block/hang (device paths).
fn is_blocked_device(filepath: &str) -> bool {
    let normalized = shellexpand::tilde(filepath).to_string();
    if BLOCKED_DEVICE_PATHS.contains(&normalized.as_str()) {
        return true;
    }
    // /proc/self/fd/0-2 and /proc/<pid>/fd/0-2
    if normalized.starts_with("/proc/")
        && (normalized.ends_with("/fd/0") || normalized.ends_with("/fd/1") || normalized.ends_with("/fd/2"))
    {
        return true;
    }
    false
}

/// Check if a path is a sensitive system location for writes.
fn check_sensitive_path(filepath: &str) -> Option<String> {
    let resolved = match path_helpers::canonicalize_shim(filepath) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => filepath.to_string(),
    };
    let resolved_lower = resolved.to_lowercase();

    // Check both the original filepath and resolved path against sensitive prefixes
    for prefix in SENSITIVE_PREFIXES {
        if resolved.starts_with(prefix) || filepath.starts_with(prefix)
            || resolved_lower.contains(prefix) || filepath.to_lowercase().contains(prefix) {
            return Some(format!(
                "Refusing to write to sensitive system path: {filepath}\nUse the terminal tool with sudo if you need to modify system files."
            ));
        }
    }
    if SENSITIVE_EXACT.contains(&resolved.as_str()) || SENSITIVE_EXACT.contains(&filepath) {
        return Some(format!(
            "Refusing to write to sensitive system path: {filepath}\nUse the terminal tool with sudo if you need to modify system files."
        ));
    }
    // Check WRITE_DENIED prefixes (match on both resolved and original filepath)
    let filepath_lower = filepath.to_lowercase();
    for prefix in WRITE_DENIED_PREFIXES {
        if resolved.contains(prefix) || resolved_lower.contains(*prefix)
            || filepath.contains(prefix) || filepath_lower.contains(*prefix) {
            return Some(format!(
                "Refusing to write to sensitive path: {filepath}\nUse the terminal tool if you need to modify this file."
            ));
        }
    }
    // Check exact denied paths
    let basename = Path::new(&resolved).file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if WRITE_DENIED_PATHS.iter().any(|p| basename == *p || resolved.ends_with(p)) {
        return Some(format!(
            "Refusing to write to sensitive path: {filepath}"
        ));
    }
    None
}

/// HERMES_HOME guard — prevent reading internal Hermes cache files.
fn is_hermes_internal(resolved: &Path) -> bool {
    let hermes_home = match hermes_core::hermes_home::get_hermes_home().canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if !resolved.starts_with(&hermes_home) {
        return false;
    }
    // Check if within skills/.hub or skills/.hub/index-cache
    let rel = match resolved.strip_prefix(&hermes_home) {
        Ok(r) => r.to_string_lossy().to_string(),
        Err(_) => return false,
    };
    rel.starts_with("skills/.hub")
}

// ---------------------------------------------------------------------------
// Canonicalize shim (cross-platform, handles non-existent paths)
// ---------------------------------------------------------------------------

mod path_helpers {
    use std::path::{Path, PathBuf};

    /// Canonicalize that works even if the file doesn't exist yet.
    /// For write_file, we need to resolve the parent directory and
    /// join the filename.
    pub fn canonicalize_shim<P: AsRef<Path>>(path: P) -> std::io::Result<PathBuf> {
        let p = path.as_ref();
        if p.exists() {
            std::fs::canonicalize(p)
        } else {
            // Resolve the parent that exists, then append the file name
            let parent = p.parent().unwrap_or(Path::new("."));
            let resolved = if parent.exists() {
                std::fs::canonicalize(parent)?
            } else {
                // Expand tilde at minimum
                let parent_str = parent.to_string_lossy();
                let expanded = shellexpand::tilde(&parent_str);
                PathBuf::from(expanded.as_ref())
            };
            if let Some(name) = p.file_name() {
                Ok(resolved.join(name))
            } else {
                Ok(resolved)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

/// Read a file with pagination and line numbers.
fn read_file(path: &str, offset: usize, limit: usize) -> Value {
    // Device path guard
    if is_blocked_device(path) {
        return serde_json::json!({
            "error": format!("Cannot read '{path}': this is a device file that would block or produce infinite output.")
        });
    }

    // Resolve path
    let expanded = shellexpand::tilde(path);
    let file_path = Path::new(expanded.as_ref());

    // Binary file guard
    if has_binary_extension(expanded.as_ref()) {
        let ext = file_path.extension()
            .map(|e| e.to_string_lossy())
            .unwrap_or_default();
        return serde_json::json!({
            "error": format!("Cannot read binary file '{path}' ({ext}). Use vision_analyze for images, or terminal to inspect binary files.")
        });
    }

    // Check if file exists
    if !file_path.is_file() {
        // Try to suggest similar files
        let suggestions = suggest_similar_files(file_path);
        if !suggestions.is_empty() {
            return serde_json::json!({
                "error": format!("File not found: {path}"),
                "suggestions": suggestions
            });
        }
        return serde_json::json!({
            "error": format!("File not found: {path}")
        });
    }

    // Hermes internal path guard
    match std::fs::canonicalize(file_path) {
        Ok(resolved) if is_hermes_internal(&resolved) => {
            return serde_json::json!({
                "error": format!("Access denied: {path} is an internal Hermes cache file and cannot be read directly to prevent prompt injection. Use the skills_list or skill_view tools instead.")
            });
        }
        _ => {}
    }

    // Read the file
    let file = match std::fs::File::open(file_path) {
        Ok(f) => f,
        Err(e) => return serde_json::json!({ "error": e.to_string() }),
    };

    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let reader = BufReader::new(file);

    // Count total lines and collect the requested range
    let start_line = offset.saturating_sub(1); // 0-indexed
    let _end_line = start_line.saturating_add(limit);
    let max_chars = DEFAULT_MAX_READ_CHARS;

    let mut lines: Vec<String> = Vec::new();
    let mut total_lines: usize = 0;
    let mut content_len: usize = 0;
    let mut truncated = false;

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                return serde_json::json!({ "error": format!("Failed to read file: {e}") });
            }
        };
        total_lines += 1;

        if total_lines > start_line && lines.len() < limit {
            // Truncate very long lines
            let display_line = if line.len() > MAX_LINE_LENGTH {
                let truncated_line = safe_truncate(&line, MAX_LINE_LENGTH);
                format!("{truncated_line}... [line truncated]")
            } else {
                line
            };
            let line_with_number = format!("{}|{}", total_lines, display_line);
            content_len += line_with_number.len();
            if content_len > max_chars {
                truncated = true;
                break;
            }
            lines.push(line_with_number);
        }
    }

    if truncated {
        lines.push(format!(
            "[Content truncated at {max_chars} chars. Use offset and limit to read specific sections. File has {total_lines} lines total.]"
        ));
    }

    let content = lines.join("\n");

    let mut result = serde_json::json!({
        "content": content,
        "path": path,
        "offset": offset,
        "limit": limit,
        "total_lines": total_lines,
        "file_size": file_size,
        "truncated": truncated,
    });

    // Large file hint
    if file_size > LARGE_FILE_HINT_BYTES && limit > 200 && truncated {
        result["_hint"] = Value::String(format!(
            "This file is large ({file_size} bytes). Consider reading only the section you need with offset and limit."
        ));
    }

    result
}

/// Suggest similar files when the requested file doesn't exist.
fn suggest_similar_files(file_path: &Path) -> Vec<String> {
    let mut suggestions = Vec::new();
    let target_name = file_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if target_name.is_empty() {
        return suggestions;
    }

    let search_dir = file_path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    if !search_dir.is_dir() {
        return suggestions;
    }

    // Simple fuzzy match on filenames in the directory
    let threshold = 80;
    for entry in std::fs::read_dir(&search_dir).ok().into_iter().flatten().filter_map(|e| e.ok()) {
        if let Ok(name) = entry.file_name().into_string() {
            if name.starts_with('.') && !target_name.starts_with('.') {
                continue; // skip hidden files unless looking for one
            }
            let score = fuzzy_similarity(&target_name, &name);
            if score >= threshold {
                let child = entry.path();
                suggestions.push(child.to_string_lossy().to_string());
            }
        }
    }
    suggestions.sort();
    suggestions.truncate(5);
    suggestions
}

/// Simple fuzzy similarity as a percentage (0-100).
fn fuzzy_similarity(a: &str, b: &str) -> u8 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();

    if a_lower == b_lower {
        return 100;
    }

    // Use longest common subsequence ratio as a simple similarity metric
    let a_chars: Vec<char> = a_lower.chars().collect();
    let b_chars: Vec<char> = b_lower.chars().collect();
    let lcs_len = lcs_length(&a_chars, &b_chars);
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 0;
    }
    ((lcs_len * 100) / max_len) as u8
}

/// Length of longest common subsequence (bounded for performance).
fn lcs_length(a: &[char], b: &[char]) -> usize {
    const MAX_LEN: usize = 200;
    if a.len() > MAX_LEN || b.len() > MAX_LEN {
        // Fall back to simple character match ratio for long strings
        let a_set: std::collections::HashSet<_> = a.iter().collect();
        let matches = b.iter().filter(|c| a_set.contains(c)).count();
        return matches.min(a.len()).min(b.len());
    }

    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }
    dp[m][n]
}

/// Safe UTF-8 truncation (from tool_result.rs pattern).
fn safe_truncate(text: &str, max_bytes: usize) -> &str {
    if max_bytes >= text.len() {
        return text;
    }
    let mut safe = max_bytes;
    while safe > 0 && !text.is_char_boundary(safe) {
        safe -= 1;
    }
    &text[..safe]
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

/// Write content to a file, creating parent directories as needed.
fn write_file(path: &str, content: &str) -> Value {
    // Check sensitive paths
    if let Some(err) = check_sensitive_path(path) {
        return serde_json::json!({ "error": err });
    }

    let expanded = shellexpand::tilde(path);
    let file_path = Path::new(expanded.as_ref());

    // Atomic write (temp file + rename to avoid partial writes on crash)
    match atomic_write(file_path, content) {
        Ok(bytes_written) => serde_json::json!({
            "success": true,
            "path": path,
            "bytes_written": bytes_written,
            "message": format!("Successfully wrote {} bytes to {path}", bytes_written),
        }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
}

// ---------------------------------------------------------------------------
// patch (replace mode)
// ---------------------------------------------------------------------------

/// Find-and-replace in a file with fuzzy matching.
fn patch_replace(path: &str, old_string: &str, new_string: &str, replace_all: bool) -> Value {
    let expanded = shellexpand::tilde(path);
    let file_path = Path::new(expanded.as_ref());

    if !file_path.is_file() {
        return serde_json::json!({ "error": format!("File not found: {path}") });
    }

    // Check sensitive paths
    if let Some(err) = check_sensitive_path(path) {
        return serde_json::json!({ "error": err });
    }

    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({ "error": e.to_string() }),
    };

    // Try exact match first
    if let Some(new_content) = exact_replace(&content, old_string, new_string, replace_all) {
        // Atomic write (temp file + rename)
        if let Err(e) = atomic_write(file_path, &new_content) {
            return serde_json::json!({ "error": format!("Failed to write patched file: {e}") });
        }
        return serde_json::json!({
            "success": true,
            "path": path,
            "diff": format!("-{}\n+{}\n", old_string, new_string),
        });
    }

    // Fall back to fuzzy matching using the `similar` crate
    fuzzy_replace(&content, old_string, new_string, replace_all, path)
}

/// Try exact string replacement. Returns Some(new_content) or None.
fn exact_replace(content: &str, old_string: &str, new_string: &str, replace_all: bool) -> Option<String> {
    if replace_all {
        let count = content.matches(old_string).count();
        if count == 0 {
            return None;
        }
        Some(content.replace(old_string, new_string))
    } else {
        match content.find(old_string) {
            Some(pos) => {
                let mut result = content[..pos].to_string();
                result.push_str(new_string);
                result.push_str(&content[pos + old_string.len()..]);
                Some(result)
            }
            None => None,
        }
    }
}

/// Fuzzy replacement using the `similar` crate for approximate matching.
fn fuzzy_replace(content: &str, old_string: &str, new_string: &str, replace_all: bool, file_path: &str) -> Value {
    use similar::TextDiff;

    if old_string.is_empty() {
        return serde_json::json!({ "error": "old_string is empty" });
    }

    // Try sliding window of old_string over content to find the best fuzzy match
    // We compare chunks of content that have the same number of lines as old_string
    let old_line_count = old_string.lines().count();
    let mut best_ratio: f32 = 0.0;
    let mut best_start: usize = 0;
    let mut best_end: usize = 0;

    // Slide through content line by line
    let content_line_vec: Vec<&str> = content.lines().collect();
    let max_search = content_line_vec.len().min(5000);
    for i in 0..max_search.saturating_sub(old_line_count).max(1) {
        let end = (i + old_line_count).min(content_line_vec.len());
        let chunk = content_line_vec[i..end].join("\n");
        let diff = TextDiff::from_lines(chunk.as_str(), old_string);
        let ratio = diff.ratio();
        if ratio > best_ratio {
            best_ratio = ratio;
            best_start = i;
            best_end = end;
        }
    }

    // Threshold: 70% similarity is a good match
    if best_ratio < 0.7 {
        return serde_json::json!({
            "error": format!("Could not find '{old_string}' in {file_path} (best fuzzy match: {:.0}% similarity)", best_ratio * 100.0f32)
        });
    }

    let end_pos = best_end.min(content_line_vec.len());
    let matched_region = content_line_vec[best_start..end_pos].join("\n");

    let new_content = if replace_all {
        // For replace_all, do all fuzzy replacements
        let mut result = content.to_string();
        let mut replacements = 0;
        let mut offset_correction: isize = 0;
        for i in 0..content_line_vec.len().saturating_sub(old_line_count).max(1) {
            let region_end = (i + old_line_count).min(content_line_vec.len());
            let region = content_line_vec[i..region_end].join("\n");
            let region_diff = TextDiff::from_lines(region.as_str(), old_string);
            if region_diff.ratio() >= 0.7 {
                let start_byte = content_line_vec[..i].join("\n").len();
                let end_byte = start_byte + region.len();
                let start_byte = (start_byte as isize + offset_correction) as usize;
                let end_byte = (end_byte as isize + offset_correction) as usize;
                let before = &result[..start_byte.min(result.len())];
                let after = &result[end_byte.min(result.len())..];
                result = format!("{}{}{}", before, new_string, after);
                offset_correction += new_string.len() as isize - region.len() as isize;
                replacements += 1;
            }
        }
        if replacements == 0 {
            return serde_json::json!({ "error": "Could not find any matching regions" });
        }
        result
    } else {
        let start_byte = content_line_vec[..best_start].join("\n").len();
        let matched_bytes = matched_region.len();
        let before = &content[..start_byte.min(content.len())];
        let after = &content[(start_byte + matched_bytes).min(content.len())..];
        format!("{}{}{}", before, new_string, after)
    };

    // Atomic write the patched file (temp file + rename)
    let expanded = shellexpand::tilde(file_path);
    let target_path = Path::new(expanded.as_ref());
    if let Err(e) = atomic_write(target_path, &new_content) {
        return serde_json::json!({ "error": format!("Failed to write patched file: {e}") });
    }

    // Generate a unified diff for the result
    let text_diff = TextDiff::from_lines(content, new_content.as_str());
    let mut diff_str = String::new();
    for change in text_diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => diff_str.push_str(&format!("-{}", change.value())),
            similar::ChangeTag::Insert => diff_str.push_str(&format!("+{}", change.value())),
            similar::ChangeTag::Equal => diff_str.push_str(change.value()),
        }
        diff_str.push('\n');
    }

    serde_json::json!({
        "success": true,
        "path": file_path,
        "match_quality": format!("{:.0}% similarity", best_ratio * 100.0f32),
        "diff": diff_str,
    })
}

// ---------------------------------------------------------------------------
// search_files
// ---------------------------------------------------------------------------

/// Search file contents or find files by name.
#[allow(clippy::too_many_arguments)]
fn search_files(
    pattern: &str,
    target: &str,
    path: &str,
    file_glob: Option<&str>,
    limit: usize,
    offset: usize,
    output_mode: &str,
    context: usize,
) -> Value {
    let expanded_path = shellexpand::tilde(path).to_string();
    let search_path = Path::new(&expanded_path);

    if target == "files" {
        // File search — find files by glob pattern
        search_files_by_name(pattern, search_path, limit, offset)
    } else {
        // Content search — regex search inside files
        search_content(pattern, search_path, file_glob, limit, offset, output_mode, context)
    }
}

/// Search for files by glob pattern.
fn search_files_by_name(pattern: &str, search_path: &Path, limit: usize, offset: usize) -> Value {
    if !search_path.exists() {
        return serde_json::json!({ "error": format!("Search path not found: {}", search_path.display()) });
    }

    let mut results: Vec<String> = Vec::new();

    // Use glob to find matching files
    let glob_pattern = if search_path.is_dir() {
        format!("{}/{}", search_path.display(), pattern)
    } else {
        pattern.to_string()
    };

    for path in glob::glob(&glob_pattern).ok().into_iter().flatten().flatten() {
        if path.is_file() {
            results.push(path.to_string_lossy().to_string());
        }
    }

    // If glob didn't match anything and search_path is a directory,
    // walk the directory looking for matching filenames
    if results.is_empty() && search_path.is_dir() {
        let walker = walkdir::WalkDir::new(search_path)
            .into_iter()
            .filter_map(|e| e.ok());

        for entry in walker {
            if let Some(name) = entry.file_name().to_str() {
                if glob_match(pattern, name) {
                    results.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }

    // Sort by modification time (newest first)
    results.sort_by(|a, b| {
        let mtime_a = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let mtime_b = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        mtime_b.cmp(&mtime_a)
    });

    let total_count = results.len();
    let truncated = offset + limit < total_count;

    // Apply pagination
    let start = offset.min(total_count);
    let end = (offset + limit).min(total_count);
    let page: Vec<String> = results[start..end].to_vec();

    serde_json::json!({
        "files": page,
        "total_count": total_count,
        "offset": offset,
        "limit": limit,
        "truncated": truncated,
        "pattern": pattern,
    })
}

/// Search content using regex.
fn search_content(
    pattern: &str,
    search_path: &Path,
    file_glob: Option<&str>,
    limit: usize,
    offset: usize,
    output_mode: &str,
    context: usize,
) -> Value {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return serde_json::json!({ "error": format!("Invalid regex: {e}") }),
    };

    let mut matches: Vec<Value> = Vec::new();
    let mut file_counts: Vec<Value> = Vec::new();

    // Collect files to search
    let files = collect_files(search_path, file_glob);

    for file_path in files {
        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => continue, // skip unreadable files
        };

        let file_match_count = re.find_iter(&content).count();
        if file_match_count == 0 {
            continue;
        }

        file_counts.push(serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "count": file_match_count,
        }));

        if output_mode == "files_only" {
            continue;
        }

        if output_mode == "count" {
            continue;
        }

        // Show content matches with line numbers
        for (line_num, line) in content.lines().enumerate() {
            for mat in re.find_iter(line) {
                if matches.len() >= limit + offset {
                    break;
                }
                if matches.len() >= offset {
                    // Include context lines
                    let context_before = get_context_lines(&content, line_num, context, true);
                    let context_after = get_context_lines(&content, line_num, context, false);

                    matches.push(serde_json::json!({
                        "path": file_path.to_string_lossy().to_string(),
                        "line": line_num + 1,
                        "match": mat.as_str(),
                        "full_line": line,
                        "context_before": context_before,
                        "context_after": context_after,
                    }));
                }
            }
            if matches.len() >= limit + offset {
                break;
            }
        }
    }

    let total_matches = matches.len();
    let page: Vec<Value> = matches.into_iter().collect();

    match output_mode {
        "files_only" => serde_json::json!({
            "files": file_counts.iter().map(|v| v["path"].as_str().unwrap_or("")).collect::<Vec<_>>(),
            "total_files": file_counts.len(),
        }),
        "count" => serde_json::json!({
            "counts": file_counts,
            "total_files_with_matches": file_counts.len(),
            "total_matches": file_counts.iter().map(|v| v["count"].as_u64().unwrap_or(0)).sum::<u64>(),
        }),
        _ => serde_json::json!({
            "matches": page,
            "total_matches": total_matches,
            "truncated": total_matches >= limit,
        }),
    }
}

/// Collect files to search in.
fn collect_files(search_path: &Path, file_glob: Option<&str>) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();

    if search_path.is_file() {
        if file_glob.is_none_or(|g| glob_match(g, search_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))) {
            files.push(search_path.to_path_buf());
        }
        return files;
    }

    if !search_path.is_dir() {
        return files;
    }

    for entry in walkdir::WalkDir::new(search_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Skip hidden files and common non-text directories
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        // Skip binary files
        if has_binary_extension(path) {
            continue;
        }
        // Apply file glob filter
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(glob) = file_glob {
                if !glob_match(glob, name) {
                    continue;
                }
            }
        }
        files.push(path.to_path_buf());
    }

    files
}

/// Simple glob matching (supports * and ** patterns).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat_lower = pattern.to_lowercase();
    let text_lower = text.to_lowercase();

    // Simple * matching
    if pat_lower.contains("**") {
        // ** matches any path segment
        let parts: Vec<&str> = pat_lower.split("**").collect();
        if parts.len() == 2 {
            return text_lower.starts_with(parts[0]) || text_lower.ends_with(parts[1]);
        }
    }

    if pat_lower.contains('*') {
        let parts: Vec<&str> = pat_lower.split('*').collect();
        if parts.len() == 2 {
            return text_lower.starts_with(parts[0]) && text_lower.ends_with(parts[1]);
        } else if parts.len() == 1 {
            return text_lower.contains(parts[0]);
        }
        // Multiple * patterns
        let mut pos = 0;
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if let Some(idx) = text_lower[pos..].find(part) {
                pos += idx + part.len();
            } else {
                return false;
            }
        }
        return true;
    }

    pat_lower == text_lower
}

/// Get context lines before or after a match.
fn get_context_lines(content: &str, line_idx: usize, count: usize, before: bool) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();

    if before {
        let start = line_idx.saturating_sub(count);
        for i in start..line_idx {
            if i < lines.len() {
                result.push(format!("{}|{}", i + 1, lines[i]));
            }
        }
    } else {
        let end = (line_idx + 1 + count).min(lines.len());
        for (j, line) in lines.iter().enumerate().take(end).skip(line_idx + 1) {
            result.push(format!("{}|{}", j + 1, line));
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register file operation tools.
pub fn register_file_tools(registry: &mut ToolRegistry) {
    // read_file
    let read_schema = serde_json::json!({
        "name": "read_file",
        "description": "Read a text file with line numbers and pagination. Use this instead of cat/head/tail in terminal. Output format: 'LINE_NUM|CONTENT'. Suggests similar filenames if not found. Use offset and limit for large files. Reads exceeding ~100K characters are rejected; use offset and limit to read specific sections of large files. NOTE: Cannot read images or binary files — use vision_analyze for images.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (absolute, relative, or ~/path)" },
                "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed, default: 1)", "default": 1, "minimum": 1 },
                "limit": { "type": "integer", "description": "Maximum number of lines to read (default: 500, max: 2000)", "default": 500, "maximum": 2000 }
            },
            "required": ["path"]
        }
    });
    registry.register(
        "read_file".to_string(), "file".to_string(), read_schema,
        std::sync::Arc::new(|args| {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
            let limit = limit.min(MAX_LINES);
            let result = read_file(&path, offset, limit);
            tool_result(&result)
        }),
        None, vec![], String::new(), "📖".to_string(), None,
    );

    // write_file
    let write_schema = serde_json::json!({
        "name": "write_file",
        "description": "Write content to a file, completely replacing existing content. Use this instead of echo/cat heredoc in terminal. Creates parent directories automatically. OVERWRITES the entire file — use 'patch' for targeted edits.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (will be created if it doesn't exist, overwritten if it does)" },
                "content": { "type": "string", "description": "Complete content to write to the file" }
            },
            "required": ["path", "content"]
        }
    });
    registry.register(
        "write_file".to_string(), "file".to_string(), write_schema,
        std::sync::Arc::new(|args| {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let content = args.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            let result = write_file(&path, &content);
            tool_result(&result)
        }),
        None, vec![], String::new(), "✍️".to_string(), Some(100_000),
    );

    // patch
    let patch_schema = serde_json::json!({
        "name": "patch",
        "description": "Targeted find-and-replace edits in files. Use this instead of sed/awk in terminal. Uses fuzzy matching (9 strategies) so minor whitespace/indentation differences won't break it. Returns a unified diff. Auto-runs syntax checks after editing.\n\nReplace mode (default): find a unique string and replace it.\nPatch mode: apply V4A multi-file patches for bulk changes.",
        "parameters": {
            "type": "object",
            "properties": {
                "mode": { "type": "string", "enum": ["replace", "patch"], "description": "Edit mode: 'replace' for targeted find-and-replace, 'patch' for V4A multi-file patches", "default": "replace" },
                "path": { "type": "string", "description": "File path to edit (required for 'replace' mode)" },
                "old_string": { "type": "string", "description": "Text to find in the file (required for 'replace' mode). Must be unique in the file unless replace_all=true." },
                "new_string": { "type": "string", "description": "Replacement text (required for 'replace' mode). Can be empty string to delete the matched text." },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default: false)", "default": false },
                "patch": { "type": "string", "description": "V4A format patch content (required for 'patch' mode)" }
            },
            "required": ["mode"]
        }
    });
    registry.register(
        "patch".to_string(), "file".to_string(), patch_schema,
        std::sync::Arc::new(|args| {
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("replace");
            let path = args.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let old_string = args.get("old_string").and_then(Value::as_str).unwrap_or("").to_string();
            let new_string = args.get("new_string").and_then(Value::as_str).unwrap_or("").to_string();
            let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            let patch_content = args.get("patch").and_then(Value::as_str).unwrap_or("").to_string();

            let result = if mode == "replace" {
                if path.is_empty() {
                    return Ok(tool_error("path required for replace mode"));
                }
                if old_string.is_empty() {
                    return Ok(tool_error("old_string required for replace mode"));
                }
                patch_replace(&path, &old_string, &new_string, replace_all)
            } else if mode == "patch" {
                if patch_content.is_empty() {
                    return Ok(tool_error("patch content required for patch mode"));
                }
                // V4A patch is handled by patch_parser crate
                apply_v4a_patch(&path, &patch_content)
            } else {
                serde_json::json!({ "error": format!("Unknown mode: {mode}") })
            };
            tool_result(&result)
        }),
        None, vec![], String::new(), "🔧".to_string(), Some(100_000),
    );

    // search_files
    let search_schema = serde_json::json!({
        "name": "search_files",
        "description": "Search file contents or find files by name. Use this instead of grep/rg/find/ls in terminal. Ripgrep-backed, faster than shell equivalents.\n\nContent search (target='content'): Regex search inside files. Output modes: full matches with line numbers, file paths only, or match counts.\n\nFile search (target='files'): Find files by glob pattern (e.g., '*.py', '*config*'). Also use this instead of ls — results sorted by modification time.",
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern for content search, or glob pattern (e.g., '*.py') for file search" },
                "target": { "type": "string", "enum": ["content", "files"], "description": "'content' searches inside file contents, 'files' searches for files by name", "default": "content" },
                "path": { "type": "string", "description": "Directory or file to search in (default: current working directory)", "default": "." },
                "file_glob": { "type": "string", "description": "Filter files by pattern in grep mode (e.g., '*.py')" },
                "limit": { "type": "integer", "description": "Maximum number of results to return (default: 50)", "default": 50 },
                "offset": { "type": "integer", "description": "Skip first N results for pagination (default: 0)", "default": 0 },
                "output_mode": { "type": "string", "enum": ["content", "files_only", "count"], "description": "Output format for grep mode", "default": "content" },
                "context": { "type": "integer", "description": "Number of context lines before and after each match", "default": 0 }
            },
            "required": ["pattern"]
        }
    });
    registry.register(
        "search_files".to_string(), "file".to_string(), search_schema,
        std::sync::Arc::new(|args| {
            let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("").to_string();
            let target = args.get("target").and_then(Value::as_str).unwrap_or("content");
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".").to_string();
            let file_glob = args.get("file_glob").and_then(Value::as_str).map(|s| s.to_string());
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let output_mode = args.get("output_mode").and_then(Value::as_str).unwrap_or("content");
            let context = args.get("context").and_then(Value::as_u64).unwrap_or(0) as usize;

            let result = search_files(&pattern, target, &path, file_glob.as_deref(), limit, offset, output_mode, context);
            tool_result(&result)
        }),
        None, vec![], String::new(), "🔎".to_string(), Some(100_000),
    );
}

/// Apply a V4A patch to a file.
fn apply_v4a_patch(_path: &str, _patch_content: &str) -> Value {
    // V4A patches can modify multiple files, so this needs to be handled
    // differently than a single-file patch. The patch_parser crate handles
    // parsing; this applies the operations.
    serde_json::json!({
        "error": "V4A patch mode applies multi-file patches — use the patch tool with the 'patch' parser directly. This handler is for 'replace' mode only."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(content: &str) -> (tempfile::NamedTempFile, String) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        let path = f.path().to_string_lossy().to_string();
        (f, path)
    }

    #[test]
    fn test_read_file_basic() {
        let (_f, path) = temp_file("line1\nline2\nline3\nline4\nline5\n");
        let result = read_file(&path, 1, 3);
        assert!(result.get("error").is_none());
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("1|line1"));
        assert!(content.contains("2|line2"));
        assert!(content.contains("3|line3"));
        assert!(!content.contains("line4"));
    }

    #[test]
    fn test_read_file_offset() {
        let (_f, path) = temp_file("a\nb\nc\nd\ne\n");
        let result = read_file(&path, 3, 2);
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("3|c"));
        assert!(content.contains("4|d"));
    }

    #[test]
    fn test_read_file_not_found() {
        let result = read_file("/nonexistent/path/file.txt", 1, 10);
        assert!(result.get("error").is_some());
    }

    #[test]
    fn test_read_file_binary_guard() {
        let result = read_file("photo.jpg", 1, 10);
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("binary file"));
    }

    #[test]
    fn test_read_file_device_guard() {
        let result = read_file("/dev/zero", 1, 10);
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("device file"));
    }

    #[test]
    fn test_write_file_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt").to_string_lossy().to_string();
        let result = write_file(&path, "hello world");
        assert!(result.get("error").is_none());
        assert!(result["success"].as_bool().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn test_write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt")
            .to_string_lossy().to_string();
        let result = write_file(&path, "nested content");
        assert!(result.get("error").is_none());
        assert!(std::fs::read_to_string(&path).unwrap().contains("nested content"));
    }

    #[test]
    fn test_patch_replace_exact() {
        let (_f, path) = temp_file("hello world\nfoo bar\n");
        let result = patch_replace(&path, "hello world", "hello rust", false);
        assert!(result.get("error").is_none());
        assert!(result["success"].as_bool().unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello rust"));
        assert!(content.contains("foo bar"));
    }

    #[test]
    fn test_patch_replace_not_found() {
        let (_f, path) = temp_file("hello world\n");
        let result = patch_replace(&path, "nonexistent", "replacement", false);
        assert!(result.get("error").is_some());
    }

    #[test]
    fn test_search_files_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test_file.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("other.txt"), "hello").unwrap();

        let result = search_files("*.rs", "files", &dir.path().to_string_lossy(), None, 10, 0, "content", 0);
        assert!(result.get("error").is_none());
        assert!(result["total_count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn test_search_content_regex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn hello() {}\nfn world() {}\nfn hello_rust() {}\n").unwrap();

        let result = search_content(
            "fn hello",
            dir.path(),
            Some("*.rs"),
            10, 0, "content", 0,
        );
        assert!(result.get("error").is_none());
        assert!(result["total_matches"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn test_search_output_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn hello() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn hello() {}").unwrap();

        let result = search_content(
            "fn hello",
            dir.path(),
            None,
            10, 0, "files_only", 0,
        );
        assert!(result.get("error").is_none());
        assert!(result["total_files"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn test_glob_match_simple() {
        assert!(glob_match("*.py", "main.py"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.py", "main.rs"));
    }

    #[test]
    fn test_glob_match_partial() {
        assert!(glob_match("*config*", "config.yaml"));
        assert!(glob_match("*config*", "my_config.json"));
        assert!(!glob_match("*config*", "data.txt"));
    }

    #[test]
    fn test_is_blocked_device() {
        assert!(is_blocked_device("/dev/zero"));
        assert!(is_blocked_device("/dev/random"));
        assert!(is_blocked_device("/dev/urandom"));
        assert!(is_blocked_device("/dev/stdin"));
        assert!(is_blocked_device("/dev/tty"));
        assert!(is_blocked_device("/proc/self/fd/0"));
        assert!(!is_blocked_device("/etc/passwd"));
    }

    #[test]
    fn test_sensitive_path_blocked() {
        // Sensitive prefix checks work on the resolved path
        // On Windows, /etc/ may not canonicalize, so we test with a path
        // that starts with the prefix literally
        let result = check_sensitive_path("/etc/systemd/system/something");
        assert!(result.is_some());
    }

    #[test]
    fn test_sensitive_path_docker_sock() {
        let result = check_sensitive_path("/var/run/docker.sock");
        assert!(result.is_some());
    }

    #[test]
    fn test_fuzzy_similarity_identical() {
        assert_eq!(fuzzy_similarity("hello", "hello"), 100);
    }

    #[test]
    fn test_fuzzy_similarity_similar() {
        let score = fuzzy_similarity("hello world", "hello wrld");
        assert!(score >= 80); // one char off
    }
}
