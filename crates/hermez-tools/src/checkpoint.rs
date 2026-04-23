#![allow(dead_code)]
//! Checkpoint Manager — Transparent filesystem snapshots via shadow git repos.
//!
//! Creates automatic snapshots of working directories before file-mutating
//! operations (write_file, patch), triggered once per conversation turn.
//! Provides rollback to any previous checkpoint.
//!
//! This is NOT a tool — the LLM never sees it. It's transparent infrastructure
//! controlled by the `checkpoints` config flag or `--checkpoints` CLI flag.
//!
//! Architecture:
//!     ~/.hermez/checkpoints/{sha256(abs_dir)[:16]}/   — shadow git repo
//!         HEAD, refs/, objects/                        — standard git internals
//!         HERMEZ_WORKDIR                               — original dir path
//!         info/exclude                                 — default excludes
//!
//! Mirrors the Python `tools/checkpoint_manager.py`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use sha2::{Digest, Sha256};

use hermez_core::hermez_home::get_hermez_home;

/// Default excludes for shadow git repos.
const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules/",
    "dist/",
    "build/",
    ".env",
    ".env.*",
    ".env.local",
    ".env.*.local",
    "__pycache__/",
    "*.pyc",
    "*.pyo",
    ".DS_Store",
    "*.log",
    ".cache/",
    ".next/",
    ".nuxt/",
    "coverage/",
    ".pytest_cache/",
    ".venv/",
    "venv/",
    ".git/",
];

/// Max files to snapshot — skip huge directories to avoid slowdowns.
const MAX_FILES: usize = 50_000;

/// Git subprocess timeout in seconds.
const GIT_TIMEOUT_SECS: u64 = 30;

/// A single checkpoint entry.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointEntry {
    pub hash: String,
    pub short_hash: String,
    pub timestamp: String,
    pub reason: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Result of a diff operation.
#[derive(Debug, Serialize)]
pub struct DiffResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stat: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub diff: String,
}

/// Result of a restore operation.
#[derive(Debug, Serialize)]
pub struct RestoreResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Manages automatic filesystem checkpoints.
///
/// Designed to be owned by AIAgent. Call `new_turn()` at the start of
/// each conversation turn and `ensure_checkpoint(dir, reason)` before
/// any file-mutating tool call. The manager deduplicates so at most one
/// snapshot is taken per directory per turn.
pub struct CheckpointManager {
    /// Master switch (from config / CLI flag).
    pub enabled: bool,
    /// Keep at most this many checkpoints per directory.
    pub max_snapshots: usize,
    /// Directories already checkpointed this turn.
    checkpointed_dirs: HashSet<String>,
    /// Whether git is available (lazy probe).
    git_available: Option<bool>,
}

impl CheckpointManager {
    /// Create a new checkpoint manager.
    pub fn new(enabled: bool, max_snapshots: usize) -> Self {
        Self {
            enabled,
            max_snapshots,
            checkpointed_dirs: HashSet::new(),
            git_available: None,
        }
    }

    /// Reset per-turn dedup. Call at the start of each agent iteration.
    pub fn new_turn(&mut self) {
        self.checkpointed_dirs.clear();
    }

    /// Take a checkpoint if enabled and not already done this turn.
    ///
    /// Returns `true` if a checkpoint was taken, `false` otherwise.
    /// Never returns an error — all errors are silently logged.
    pub async fn ensure_checkpoint(&mut self, working_dir: &str, reason: &str) -> bool {
        if !self.enabled {
            return false;
        }

        // Lazy git probe
        if self.git_available.is_none() {
            let available = which_git().await;
            self.git_available = Some(available);
            if !available {
                tracing::debug!("Checkpoints disabled: git not found");
            }
        }
        if !self.git_available.unwrap_or(false) {
            return false;
        }

        let abs_dir = match Path::new(working_dir).canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => return false,
        };

        // Skip root, home, and other overly broad directories
        if abs_dir == "/" || abs_dir == home_dir_string() {
            tracing::debug!("Checkpoint skipped: directory too broad ({})", abs_dir);
            return false;
        }

        // Already checkpointed this turn?
        if self.checkpointed_dirs.contains(&abs_dir) {
            return false;
        }

        self.checkpointed_dirs.insert(abs_dir.clone());

        match self.take_snapshot(&abs_dir, reason).await {
            Ok(taken) => taken,
            Err(e) => {
                tracing::debug!("Checkpoint failed (non-fatal): {}", e);
                false
            }
        }
    }

    /// List available checkpoints for a directory.
    pub async fn list_checkpoints(&self, working_dir: &str) -> Vec<CheckpointEntry> {
        let abs_dir = match Path::new(working_dir).canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => return Vec::new(),
        };

        let shadow = shadow_repo_path(&abs_dir);
        if !shadow.join("HEAD").exists() {
            return Vec::new();
        }

        // Get log output
        let ok = run_git(
            &[
                "log",
                "--format=%H|%h|%aI|%s",
                "-n",
                &self.max_snapshots.to_string(),
            ],
            &shadow,
            &abs_dir,
        )
        .await;

        let stdout = match ok {
            Ok((_, out, _)) if !out.is_empty() => out,
            _ => return Vec::new(),
        };

        let mut results = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() == 4 {
                let mut entry = CheckpointEntry {
                    hash: parts[0].to_string(),
                    short_hash: parts[1].to_string(),
                    timestamp: parts[2].to_string(),
                    reason: parts[3].to_string(),
                    files_changed: 0,
                    insertions: 0,
                    deletions: 0,
                };

                // Get diffstat for this commit
                if let Ok((_, stat_out, _)) = run_git(
                    &["diff", "--shortstat", &format!("{}~1", parts[0]), parts[0]],
                    &shadow,
                    &abs_dir,
                )
                .await
                {
                    if !stat_out.is_empty() {
                        parse_shortstat(&stat_out, &mut entry);
                    }
                }

                results.push(entry);
            }
        }

        results
    }

    /// Show diff between a checkpoint and the current working tree.
    pub async fn diff(&self, working_dir: &str, commit_hash: &str) -> DiffResult {
        let abs_dir = match Path::new(working_dir).canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => {
                return DiffResult {
                    success: false,
                    error: Some("Invalid directory".to_string()),
                    stat: String::new(),
                    diff: String::new(),
                }
            }
        };

        let shadow = shadow_repo_path(&abs_dir);
        if !shadow.join("HEAD").exists() {
            return DiffResult {
                success: false,
                error: Some("No checkpoints exist for this directory".to_string()),
                stat: String::new(),
                diff: String::new(),
            };
        }

        // Verify the commit exists
        if run_git(&["cat-file", "-t", commit_hash], &shadow, &abs_dir)
            .await
            .is_err()
        {
            return DiffResult {
                success: false,
                error: Some(format!("Checkpoint '{}' not found", commit_hash)),
                stat: String::new(),
                diff: String::new(),
            };
        }

        // Stage current state to compare against checkpoint
        let _ = run_git(&["add", "-A"], &shadow, &abs_dir).await;

        // Get stat summary
        let stat_out = run_git(
            &["diff", "--stat", commit_hash, "--cached"],
            &shadow,
            &abs_dir,
        )
        .await
        .ok()
        .map(|(_, out, _)| out);

        // Get actual diff
        let diff_out = run_git(
            &["diff", commit_hash, "--cached", "--no-color"],
            &shadow,
            &abs_dir,
        )
        .await
        .ok()
        .map(|(_, out, _)| out);

        // Unstage — must succeed to avoid corrupting future diffs/checkpoints
        if let Err(e) = run_git(&["reset", "HEAD"], &shadow, &abs_dir).await {
            tracing::error!(
                "checkpoint: failed to unstage shadow repo after diff: {:?} — future diffs may be corrupted",
                e
            );
        }

        if stat_out.is_none() && diff_out.is_none() {
            return DiffResult {
                success: false,
                error: Some("Could not generate diff".to_string()),
                stat: String::new(),
                diff: String::new(),
            };
        }

        DiffResult {
            success: true,
            error: None,
            stat: stat_out.unwrap_or_default(),
            diff: diff_out.unwrap_or_default(),
        }
    }

    /// Restore files to a checkpoint state.
    pub async fn restore(&mut self, working_dir: &str, commit_hash: &str, file_path: Option<&str>) -> RestoreResult {
        let abs_dir = match Path::new(working_dir).canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => {
                return RestoreResult {
                    success: false,
                    error: Some("Invalid directory".to_string()),
                    restored_to: None,
                    reason: None,
                    directory: None,
                    file: None,
                }
            }
        };

        let shadow = shadow_repo_path(&abs_dir);
        if !shadow.join("HEAD").exists() {
            return RestoreResult {
                success: false,
                error: Some("No checkpoints exist for this directory".to_string()),
                restored_to: None,
                reason: None,
                directory: None,
                file: None,
            };
        }

        // Verify the commit exists
        if run_git(&["cat-file", "-t", commit_hash], &shadow, &abs_dir)
            .await
            .is_err()
        {
            return RestoreResult {
                success: false,
                error: Some(format!("Checkpoint '{}' not found", commit_hash)),
                restored_to: None,
                reason: None,
                directory: None,
                file: None,
            };
        }

        // Take a checkpoint of current state before restoring
        let _ = self
            .take_snapshot(&abs_dir, &format!("pre-rollback snapshot (restoring to {})", &commit_hash[..8]))
            .await;

        // Restore — full directory or single file
        let restore_target = file_path.unwrap_or(".");
        // Security: reject path traversal sequences in restore target
        if restore_target.contains("..") {
            return RestoreResult {
                success: false,
                error: Some("Restore path contains path traversal (..) which is not allowed".to_string()),
                restored_to: None,
                reason: None,
                directory: None,
                file: file_path.map(String::from),
            };
        }
        let ok = run_git(
            &["checkout", commit_hash, "--", restore_target],
            &shadow,
            &abs_dir,
        )
        .await;

        if let Err(e) = ok {
            return RestoreResult {
                success: false,
                error: Some(format!("Restore failed: {}", e)),
                restored_to: None,
                reason: None,
                directory: None,
                file: file_path.map(String::from),
            };
        }

        // Get info about what was restored
        let reason = run_git(&["log", "--format=%s", "-1", commit_hash], &shadow, &abs_dir)
            .await
            .ok()
            .map(|(_, out, _)| out)
            .unwrap_or_else(|| "unknown".to_string());

        RestoreResult {
            success: true,
            error: None,
            restored_to: Some(commit_hash[..8].to_string()),
            reason: Some(reason),
            directory: Some(abs_dir),
            file: file_path.map(String::from),
        }
    }

    /// Resolve a file path to its working directory for checkpointing.
    ///
    /// Walks up from the file's parent to find a reasonable project root
    /// (directory containing .git, pyproject.toml, package.json, etc.).
    /// Falls back to the file's parent directory.
    pub fn get_working_dir_for_path(file_path: &str) -> String {
        let path = Path::new(file_path);
        let candidate = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().unwrap_or(path).to_path_buf()
        };

        // Walk up looking for project root markers
        let markers = [
            ".git",
            "pyproject.toml",
            "package.json",
            "Cargo.toml",
            "go.mod",
            "Makefile",
            "pom.xml",
            ".hg",
            "Gemfile",
        ];

        let mut check = candidate.clone();
        loop {
            if markers.iter().any(|m| check.join(m).exists()) {
                return check.to_string_lossy().to_string();
            }
            match check.parent() {
                Some(parent) if parent != check => check = parent.to_path_buf(),
                _ => break,
            }
        }

        // No project root found — use the file's parent
        candidate.to_string_lossy().to_string()
    }

    /// Internal: take a snapshot. Returns true on success.
    async fn take_snapshot(&self, working_dir: &str, reason: &str) -> Result<bool, String> {
        let shadow = shadow_repo_path(working_dir);

        // Init if needed
        if init_shadow_repo(&shadow, working_dir).await.is_err() {
            return Ok(false);
        }

        // Quick size guard
        if dir_file_count(working_dir) > MAX_FILES {
            tracing::debug!(
                "Checkpoint skipped: >{} files in {}",
                MAX_FILES,
                working_dir
            );
            return Ok(false);
        }

        // Stage everything
        if run_git(&["add", "-A"], &shadow, working_dir).await.is_err() {
            return Ok(false);
        }

        // Check if there's anything to commit
        let ok_diff = run_git(&["diff", "--cached", "--quiet"], &shadow, working_dir).await;
        if ok_diff.is_ok() {
            // No changes to commit
            tracing::debug!("Checkpoint skipped: no changes in {}", working_dir);
            return Ok(false);
        }

        // Commit
        if run_git(&["commit", "-m", reason, "--allow-empty-message"], &shadow, working_dir)
            .await
            .is_err()
        {
            return Ok(false);
        }

        tracing::debug!("Checkpoint taken in {}: {}", working_dir, reason);

        Ok(true)
    }
}

/// Compute deterministic shadow repo path: sha256(abs_path)[:16].
fn shadow_repo_path(working_dir: &str) -> PathBuf {
    let abs_path = match Path::new(working_dir).canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => working_dir.to_string(),
    };
    let mut hasher = Sha256::new();
    hasher.update(abs_path.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    get_hermez_home().join("checkpoints").join(&hash[..16])
}

/// Initialize shadow repo if needed.
async fn init_shadow_repo(shadow_repo: &Path, working_dir: &str) -> Result<(), String> {
    if shadow_repo.join("HEAD").exists() {
        return Ok(());
    }

    std::fs::create_dir_all(shadow_repo).map_err(|e| format!("mkdir failed: {}", e))?;

    run_git(&["init"], shadow_repo, working_dir).await?;
    let _ = run_git(&["config", "user.email", "hermez@local"], shadow_repo, working_dir).await;
    let _ = run_git(&["config", "user.name", "Hermez Checkpoint"], shadow_repo, working_dir).await;

    // Write exclude file
    let info_dir = shadow_repo.join("info");
    let _ = std::fs::create_dir_all(&info_dir);
    let exclude_content = DEFAULT_EXCLUDES.join("\n") + "\n";
    let _ = std::fs::write(info_dir.join("exclude"), exclude_content);

    // Write HERMEZ_WORKDIR
    let _ = std::fs::write(
        shadow_repo.join("HERMEZ_WORKDIR"),
        working_dir.to_string() + "\n",
    );

    Ok(())
}

/// Run a git command against the shadow repo.
async fn run_git(
    args: &[&str],
    shadow_repo: &Path,
    working_dir: &str,
) -> Result<(bool, String, String), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args);
    cmd.current_dir(working_dir);
    cmd.env("GIT_DIR", shadow_repo);
    cmd.env("GIT_WORK_TREE", working_dir);
    cmd.env_remove("GIT_INDEX_FILE");
    cmd.env_remove("GIT_NAMESPACE");
    cmd.env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(GIT_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .map_err(|_| format!("git timed out after {}s", GIT_TIMEOUT_SECS))?
    .map_err(|e| format!("git failed: {}", e))?;

    let ok = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !ok {
        tracing::debug!(
            "Git command failed: git {} (rc={}) stderr={}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr
        );
    }

    Ok((ok, stdout, stderr))
}

/// Check if git is available on the system.
async fn which_git() -> bool {
    tokio::process::Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Quick file count estimate (stops early if over MAX_FILES).
fn dir_file_count(path: &str) -> usize {
    let mut count = 0;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .take(MAX_FILES + 1)
    {
        if entry.is_ok() {
            count += 1;
            if count > MAX_FILES {
                return count;
            }
        }
    }
    count
}

/// Parse git --shortstat output into entry dict.
fn parse_shortstat(stat_line: &str, entry: &mut CheckpointEntry) {
    if let Some(m) = capture_number(stat_line, "file") {
        entry.files_changed = m;
    }
    if let Some(m) = capture_number(stat_line, "insertion") {
        entry.insertions = m;
    }
    if let Some(m) = capture_number(stat_line, "deletion") {
        entry.deletions = m;
    }
}

fn capture_number(text: &str, keyword: &str) -> Option<usize> {
    // Use regex: find a number followed by optional whitespace and the keyword (with optional 's')
    // e.g., "3 files changed" → 3, "10 insertions(+)" → 10
    let pattern = format!(r"(\d+)\s*{}s?\b", regex::escape(keyword));
    let re = regex::Regex::new(&pattern).ok()?;
    re.captures(text)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

fn home_dir_string() -> String {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Format checkpoint list for display to user.
pub fn format_checkpoint_list(checkpoints: &[CheckpointEntry], directory: &str) -> String {
    if checkpoints.is_empty() {
        return format!("No checkpoints found for {}", directory);
    }

    let mut lines = Vec::new();
    lines.push(format!("Checkpoints for {}:\n", directory));

    for (i, cp) in checkpoints.iter().enumerate() {
        // Format timestamp
        let ts = &cp.timestamp;
        let display_ts = if let Some(t_pos) = ts.find('T') {
            let date = &ts[..t_pos];
            let time = &ts[t_pos + 1..];
            let time_short = time.split('+').next().unwrap_or(time).split('-').next().unwrap_or(time);
            let time_5 = if time_short.len() >= 5 {
                &time_short[..5]
            } else {
                time_short
            };
            format!("{} {}", date, time_5)
        } else {
            ts.to_string()
        };

        // Build change summary
        let stat = if cp.files_changed > 0 {
            let s = if cp.files_changed != 1 { "s" } else { "" };
            format!(
                "  ({} file{}, +{}/-{})",
                cp.files_changed, s, cp.insertions, cp.deletions
            )
        } else {
            String::new()
        };

        lines.push(format!(
            "  {}. {}  {}  {}{}",
            i + 1,
            cp.short_hash,
            display_ts,
            cp.reason,
            stat
        ));
    }

    lines.push("\n  /rollback <N>             restore to checkpoint N".to_string());
    lines.push("  /rollback diff <N>        preview changes since checkpoint N".to_string());
    lines.push("  /rollback <N> <file>      restore a single file from checkpoint N".to_string());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_shortstat_basic() {
        let mut entry = CheckpointEntry {
            hash: String::new(),
            short_hash: String::new(),
            timestamp: String::new(),
            reason: String::new(),
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        };
        parse_shortstat("1 file changed, 5 insertions(+), 3 deletions(-)", &mut entry);
        assert_eq!(entry.files_changed, 1);
        assert_eq!(entry.insertions, 5);
        assert_eq!(entry.deletions, 3);
    }

    #[test]
    fn test_parse_shortstat_multiple_files() {
        let mut entry = CheckpointEntry {
            hash: String::new(),
            short_hash: String::new(),
            timestamp: String::new(),
            reason: String::new(),
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        };
        parse_shortstat("3 files changed, 10 insertions(+), 2 deletions(-)", &mut entry);
        assert_eq!(entry.files_changed, 3);
        assert_eq!(entry.insertions, 10);
        assert_eq!(entry.deletions, 2);
    }

    #[test]
    fn test_shadow_repo_path_deterministic() {
        let path1 = shadow_repo_path("/tmp/test");
        let path2 = shadow_repo_path("/tmp/test");
        assert_eq!(path1, path2);
    }

    #[test]
    fn test_shadow_repo_path_different() {
        let path1 = shadow_repo_path("/tmp/test_a");
        let path2 = shadow_repo_path("/tmp/test_b");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_disabled_manager() {
        let mgr = CheckpointManager::new(false, 50);
        // Ensure checkpoint should return false immediately
        // Note: can't use async in tests easily, so test the enabled flag
        assert!(!mgr.enabled);
    }

    #[test]
    fn test_new_turn_clears() {
        let mut mgr = CheckpointManager::new(true, 50);
        mgr.checkpointed_dirs.insert("/tmp/test".to_string());
        assert_eq!(mgr.checkpointed_dirs.len(), 1);
        mgr.new_turn();
        assert!(mgr.checkpointed_dirs.is_empty());
    }

    #[test]
    fn test_capture_number() {
        assert_eq!(capture_number("5 insertions(+)", "insertion"), Some(5));
        assert_eq!(capture_number("1 file changed", "file"), Some(1));
        assert_eq!(capture_number("no changes", "file"), None);
        // Edge case: comma-separated numbers
        assert_eq!(capture_number("3 files changed, 10 insertions(+), 2 deletions(-)", "insertion"), Some(10));
        assert_eq!(capture_number("3 files changed, 10 insertions(+), 2 deletions(-)", "deletion"), Some(2));
        assert_eq!(capture_number("3 files changed, 10 insertions(+), 2 deletions(-)", "file"), Some(3));
    }
}
