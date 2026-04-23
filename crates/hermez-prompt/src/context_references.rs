#![allow(dead_code)]
//! Context reference parser.
//!
//! Handles `@file:path`, `@folder:path`, `@git`, `@url:https://...`,
//! `@diff`, `@staged` references in user messages. Expands them into
//! inline context blocks attached to the prompt.

use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};

static REFERENCE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"@(?:(?P<simple>diff|staged)\b|(?P<kind>file|folder|git|url):(?P<value>`[^`\n]+`|"[^"\n]+"|'[^'\n]+'|\S+))"#,
    )
    .unwrap()
});

/// Check if the character before `pos` is a word character or `/`.
fn is_word_char_before(s: &str, pos: usize) -> bool {
    if pos == 0 {
        return false;
    }
    s[..pos].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '/')
}

static FILE_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?P<path>.*?)(?::(?P<start>\d+)(?:-(?P<end>\d+))?)?$").unwrap());

/// A parsed context reference.
#[derive(Debug, Clone)]
pub struct ContextReference {
    /// Raw matched text (e.g. `@file:src/main.rs`).
    pub raw: String,
    /// Kind: file, folder, git, url, diff, staged.
    pub kind: String,
    /// Target path/URL.
    pub target: String,
    /// Start offset in the original message.
    pub start: usize,
    /// End offset in the original message.
    pub end: usize,
    /// Optional line range start (1-based).
    pub line_start: Option<usize>,
    /// Optional line range end (1-based).
    pub line_end: Option<usize>,
}

/// Result of preprocessing context references.
#[derive(Debug, Clone)]
pub struct ContextReferenceResult {
    /// Message with references stripped and context blocks appended.
    pub message: String,
    /// Original unmodified message.
    pub original_message: String,
    /// Parsed references.
    pub references: Vec<ContextReference>,
    /// Warnings about failed/blocked references.
    pub warnings: Vec<String>,
    /// Estimated token count of injected context.
    pub injected_tokens: usize,
    /// Whether any context was expanded.
    pub expanded: bool,
    /// Whether context injection was blocked due to token limits.
    pub blocked: bool,
}

/// Rough token estimation (character-based heuristic).
fn estimate_tokens_rough(text: &str) -> usize {
    text.chars().count() / 4
}

/// Strip trailing punctuation from a reference value.
fn strip_trailing_punctuation(s: &str) -> &str {
    s.trim_end_matches(|c: char| ",.;!?".contains(c))
}

/// Strip reference wrapper characters (backticks, quotes).
fn strip_reference_wrappers(s: &str) -> String {
    let trimmed = s.trim();
    if (trimmed.starts_with('`') && trimmed.ends_with('`'))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse all `@` context references from a message.
pub fn parse_context_references(message: &str) -> Vec<ContextReference> {
    let mut refs = Vec::new();
    for cap in REFERENCE_RE.captures_iter(message) {
        let full_match = cap.get(0).unwrap();
        let raw = full_match.as_str().to_string();
        let start = full_match.start();
        let end = full_match.end();

        // Skip if preceded by word char or / (not a real @ mention)
        if is_word_char_before(message, start) {
            continue;
        }

        if let Some(simple) = cap.name("simple") {
            refs.push(ContextReference {
                raw,
                kind: simple.as_str().to_string(),
                target: String::new(),
                start,
                end,
                line_start: None,
                line_end: None,
            });
            continue;
        }

        let kind = cap.name("kind").map(|m| m.as_str().to_string()).unwrap_or_default();
        let value = cap
            .name("value")
            .map(|m| strip_trailing_punctuation(m.as_str()))
            .unwrap_or("");
        let target = strip_reference_wrappers(value);

        let mut line_start = None;
        let mut line_end = None;
        if kind == "file" {
            if let Some(file_cap) = FILE_LINE_RE.captures(&target) {
                if let Some(s) = file_cap.name("start") {
                    line_start = s.as_str().parse::<usize>().ok();
                }
                if let Some(e) = file_cap.name("end") {
                    line_end = e.as_str().parse::<usize>().ok();
                }
                // Reconstruct target without line range for storage
                // Actually keep full target as-is for display
            }
        }

        refs.push(ContextReference {
            raw,
            kind,
            target,
            start,
            end,
            line_start,
            line_end,
        });
    }
    refs
}

/// Resolve a path relative to `cwd`, with optional root constraint.
fn resolve_path(cwd: &Path, target: &str, allowed_root: Option<&Path>) -> PathBuf {
    let path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        cwd.join(target)
    };
    // If allowed_root is set, verify path is within it
    if let Some(root) = allowed_root {
        if let Ok(resolved) = path.canonicalize() {
            if !resolved.starts_with(root) {
                return path; // return original; security check will fail downstream
            }
        }
    }
    path
}

/// Check if a path escapes the allowed directory.
fn ensure_path_allowed(path: &Path, allowed_root: &Path) -> Result<(), String> {
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("Path resolution failed: {e}"))?;
    let root_resolved = allowed_root
        .canonicalize()
        .map_err(|e| format!("Root resolution failed: {e}"))?;
    if resolved.starts_with(&root_resolved) {
        Ok(())
    } else {
        Err(format!(
            "Path escapes allowed directory: {}",
            resolved.display()
        ))
    }
}

/// Detect if a file is binary.
fn is_binary_file(path: &Path) -> bool {
    // Quick check: read first 8KB and look for null bytes
    use std::io::Read;
    if let Ok(mut file) = std::fs::File::open(path) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = file.read(&mut buf) {
            return buf[..n].contains(&0);
        }
    }
    false
}

/// Guess code fence language from file extension.
fn code_fence_language(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

/// Build a folder listing as a tree string.
fn build_folder_listing(dir: &Path, cwd: &Path) -> String {
    let mut lines = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            if path.is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        for d in &dirs {
            lines.push(format!("📁 {d}/"));
        }
        for f in &files {
            lines.push(format!("📄 {f}"));
        }
    }
    let rel = pathdiff::diff_paths(dir, cwd).unwrap_or_else(|| dir.to_path_buf());
    format!(
        "```\n{}/\n{}```",
        rel.display(),
        lines.join("\n")
    )
}

/// Fetch URL content synchronously.
fn fetch_url_content_sync(url: &str) -> Result<String, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to build runtime: {e}"))?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        resp.text()
            .await
            .map_err(|e| format!("Failed to read body: {e}"))
    })
}

/// Expand a single reference into a context block.
fn expand_reference(
    ref_: &ContextReference,
    cwd: &Path,
    allowed_root: Option<&Path>,
) -> (Option<String>, Option<String>) {
    let root = allowed_root.unwrap_or(cwd);
    match ref_.kind.as_str() {
        "file" => expand_file_reference(ref_, cwd, root),
        "folder" => expand_folder_reference(ref_, cwd, root),
        "diff" => expand_git_command(ref_, &["diff"], "git diff"),
        "staged" => expand_git_command(ref_, &["diff", "--staged"], "git diff --staged"),
        "git" => {
            let count = ref_
                .target
                .parse::<usize>()
                .ok()
                .filter(|&n| n > 0)
                .unwrap_or(1)
                .min(10);
            expand_git_command(ref_, &["log", &format!("-{count}"), "-p"], "git log")
        }
        "url" => {
            match fetch_url_content_sync(&ref_.target) {
                Ok(content) if !content.is_empty() => {
                    let tokens = estimate_tokens_rough(&content);
                    (
                        None,
                        Some(format!(
                            "🌐 {} ({tokens} tokens)\n{content}",
                            ref_.raw
                        )),
                    )
                }
                Ok(_) => (Some(format!("{}: no content extracted", ref_.raw)), None),
                Err(e) => (Some(format!("{}: {e}", ref_.raw)), None),
            }
        }
        _ => (Some(format!("{}: unsupported reference type", ref_.raw)), None),
    }
}

fn expand_file_reference(
    ref_: &ContextReference,
    cwd: &Path,
    root: &Path,
) -> (Option<String>, Option<String>) {
    // Extract base path (without line range)
    let target = &ref_.target;
    let base_path = if let Some(cap) = FILE_LINE_RE.captures(target) {
        cap.name("path")
            .map(|m| m.as_str().to_string())
            .unwrap_or(target.clone())
    } else {
        target.clone()
    };

    let path = resolve_path(cwd, &base_path, Some(root));
    if let Err(e) = ensure_path_allowed(&path, root) {
        return (Some(format!("{}: {e}", ref_.raw)), None);
    }
    if !path.exists() {
        return (Some(format!("{}: file not found", ref_.raw)), None);
    }
    if !path.is_file() {
        return (Some(format!("{}: path is not a file", ref_.raw)), None);
    }
    if is_binary_file(&path) {
        return (
            Some(format!("{}: binary files are not supported", ref_.raw)),
            None,
        );
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return (Some(format!("{}: {e}", ref_.raw)), None),
    };

    let text = if ref_.line_start.is_some() {
        let lines: Vec<&str> = text.lines().collect();
        let start_idx = ref_.line_start.unwrap_or(1).saturating_sub(1);
        let end_idx = ref_.line_end.unwrap_or(lines.len()).min(lines.len());
        if start_idx < lines.len() {
            lines[start_idx..end_idx.min(lines.len())].join("\n")
        } else {
            text
        }
    } else {
        text
    };

    let lang = code_fence_language(&path);
    let tokens = estimate_tokens_rough(&text);
    (
        None,
        Some(format!(
            "📄 {} ({tokens} tokens)\n```{lang}\n{text}\n```",
            ref_.raw
        )),
    )
}

fn expand_folder_reference(
    ref_: &ContextReference,
    cwd: &Path,
    root: &Path,
) -> (Option<String>, Option<String>) {
    let path = resolve_path(cwd, &ref_.target, Some(root));
    if let Err(e) = ensure_path_allowed(&path, root) {
        return (Some(format!("{}: {e}", ref_.raw)), None);
    }
    if !path.exists() {
        return (Some(format!("{}: folder not found", ref_.raw)), None);
    }
    if !path.is_dir() {
        return (Some(format!("{}: path is not a folder", ref_.raw)), None);
    }

    let listing = build_folder_listing(&path, cwd);
    let tokens = estimate_tokens_rough(&listing);
    (None, Some(format!("📁 {} ({tokens} tokens)\n{listing}", ref_.raw)))
}

fn expand_git_command(
    ref_: &ContextReference,
    args: &[&str],
    label: &str,
) -> (Option<String>, Option<String>) {
    let output = std::process::Command::new("git")
        .args(args)
        .output();

    match output {
        Ok(out) => {
            if out.status.success() {
                let content = String::from_utf8_lossy(&out.stdout);
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() {
                    (Some(format!("{}: {label} produced no output", ref_.raw)), None)
                } else {
                    let tokens = estimate_tokens_rough(&trimmed);
                    (None, Some(format!("🔀 {} ({tokens} tokens)\n{}\n{trimmed}", ref_.raw, label)))
                }
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                (
                    Some(format!("{}: {label} failed: {}", ref_.raw, stderr.trim())),
                    None,
                )
            }
        }
        Err(e) => (Some(format!("{}: git command failed: {e}", ref_.raw)), None),
    }
}

/// Remove reference tokens from the message, keeping surrounding text.
fn remove_reference_tokens(message: &str, refs: &[ContextReference]) -> String {
    let mut result = String::with_capacity(message.len());
    let mut last_end = 0;
    for ref_ in refs {
        if ref_.start >= last_end {
            result.push_str(&message[last_end..ref_.start]);
            last_end = ref_.end;
        }
    }
    result.push_str(&message[last_end..]);
    result.trim().to_string()
}

/// Preprocess context references in a message.
///
/// Expands `@file`, `@folder`, `@git`, `@url`, `@diff`, `@staged` references
/// into inline context blocks attached to the prompt.
pub fn preprocess_context_references(
    message: &str,
    cwd: &Path,
    context_length: usize,
    allowed_root: Option<&Path>,
) -> ContextReferenceResult {
    let refs = parse_context_references(message);
    if refs.is_empty() {
        return ContextReferenceResult {
            message: message.to_string(),
            original_message: message.to_string(),
            references: Vec::new(),
            warnings: Vec::new(),
            injected_tokens: 0,
            expanded: false,
            blocked: false,
        };
    }

    let root = allowed_root.unwrap_or(cwd);
    let mut warnings = Vec::new();
    let mut blocks = Vec::new();
    let mut injected_tokens = 0;

    for ref_ in &refs {
        let (warning, block) = expand_reference(ref_, cwd, Some(root));
        if let Some(w) = warning {
            warnings.push(w);
        }
        if let Some(b) = block {
            injected_tokens += estimate_tokens_rough(&b);
            blocks.push(b);
        }
    }

    let hard_limit = (context_length as f64 * 0.50).max(1.0) as usize;
    let soft_limit = (context_length as f64 * 0.25).max(1.0) as usize;

    if injected_tokens > hard_limit {
        warnings.push(format!(
            "@ context injection refused: {injected_tokens} tokens exceeds the 50% hard limit ({hard_limit})."
        ));
        return ContextReferenceResult {
            message: message.to_string(),
            original_message: message.to_string(),
            references: refs,
            warnings,
            injected_tokens,
            expanded: false,
            blocked: true,
        };
    }

    if injected_tokens > soft_limit {
        warnings.push(format!(
            "@ context injection warning: {injected_tokens} tokens exceeds the 25% soft limit ({soft_limit})."
        ));
    }

    let stripped = remove_reference_tokens(message, &refs);
    let mut final_msg = stripped;
    let has_warnings = !warnings.is_empty();
    if has_warnings {
        final_msg.push_str("\n\n--- Context Warnings ---\n");
        for w in &warnings {
            final_msg.push_str(&format!("- {w}\n"));
        }
    }
    let has_blocks = !blocks.is_empty();
    if has_blocks {
        final_msg.push_str("\n\n--- Attached Context ---\n\n");
        final_msg.push_str(&blocks.join("\n\n"));
    }

    ContextReferenceResult {
        message: final_msg.trim().to_string(),
        original_message: message.to_string(),
        references: refs,
        warnings,
        injected_tokens,
        expanded: has_blocks || has_warnings,
        blocked: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_reference() {
        let refs = parse_context_references("Check @file:src/main.rs for bugs");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "file");
        assert_eq!(refs[0].target, "src/main.rs");
    }

    #[test]
    fn test_parse_diff_reference() {
        let refs = parse_context_references("Show me @diff");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "diff");
        assert_eq!(refs[0].target, "");
    }

    #[test]
    fn test_parse_url_reference() {
        let refs = parse_context_references("Read @url:https://example.com");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "url");
        assert_eq!(refs[0].target, "https://example.com");
    }

    #[test]
    fn test_parse_multiple() {
        let refs = parse_context_references("@diff and @file:lib.rs");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].kind, "diff");
        assert_eq!(refs[1].kind, "file");
    }

    #[test]
    fn test_parse_no_references() {
        let refs = parse_context_references("Hello world");
        assert!(refs.is_empty());
    }

    #[test]
    fn test_strip_wrappers() {
        assert_eq!(strip_reference_wrappers("`file.txt`"), "file.txt");
        assert_eq!(strip_reference_wrappers("\"file.txt\""), "file.txt");
        assert_eq!(strip_reference_wrappers("'file.txt'"), "file.txt");
        assert_eq!(strip_reference_wrappers("file.txt"), "file.txt");
    }

    #[test]
    fn test_remove_reference_tokens() {
        let refs = parse_context_references("Hello @file:x.txt world");
        let stripped = remove_reference_tokens("Hello @file:x.txt world", &refs);
        assert_eq!(stripped, "Hello  world");
    }
}
