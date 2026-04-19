//! File sync manager for remote execution backends.
//!
//! Mirrors the Python `tools/environments/file_sync.py`.
//! Tracks local file changes via mtime+size, detects deletions, and
//! syncs to remote environments transactionally. Used by SSH, Modal,
//! and Daytona. Docker and Singularity use bind mounts (live host FS
//! view) and don't need this.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Transport callback: upload a single file (host_path, remote_path).
pub type UploadFn = Box<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

/// Transport callback: bulk upload many files [(host_path, remote_path), ...].
pub type BulkUploadFn = Option<Box<dyn Fn(&[(String, String)]) -> Result<(), String> + Send + Sync>>;

/// Transport callback: delete remote paths.
pub type DeleteFn = Box<dyn Fn(&[String]) -> Result<(), String> + Send + Sync>;

/// Transport callback: enumerate files to sync [(host_path, remote_path), ...].
pub type GetFilesFn = Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// Default sync interval (seconds).
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Environment variable that forces sync on every call.
const FORCE_SYNC_ENV: &str = "HERMES_FORCE_FILE_SYNC";

/// Get `(mtime, size)` for a file, or `None` if unreadable.
fn file_mtime_key(host_path: &str) -> Option<(f64, u64)> {
    let meta = std::fs::metadata(host_path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some((mtime, meta.len()))
}

/// Build a shell `rm -f` command for a batch of remote paths.
pub fn quoted_rm_command(remote_paths: &[String]) -> String {
    let parts: Vec<String> = remote_paths
        .iter()
        .map(|p| {
            // Simple shell quoting: wrap in single quotes, escape internal single quotes
            format!("'{}'", p.replace('\'', "'\\''"))
        })
        .collect();
    format!("rm -f {}", parts.join(" "))
}

/// Build a shell `mkdir -p` command for a batch of directories.
pub fn quoted_mkdir_command(dirs: &[String]) -> String {
    let parts: Vec<String> = dirs
        .iter()
        .map(|d| {
            format!("'{}'", d.replace('\'', "'\\''"))
        })
        .collect();
    format!("mkdir -p {}", parts.join(" "))
}

/// Extract sorted unique parent directories from (host, remote) pairs.
pub fn unique_parent_dirs(files: &[(String, String)]) -> Vec<String> {
    let mut dirs: Vec<String> = files
        .iter()
        .filter_map(|(_, remote)| Path::new(remote).parent().map(|p| p.to_string_lossy().to_string()))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Tracks local file changes and syncs to a remote environment.
///
/// Backends instantiate this with transport callbacks (upload, delete)
/// and a file-source callable. The manager handles mtime-based change
/// detection, deletion tracking, rate limiting, and transactional state.
///
/// Not used by bind-mount backends (Docker, Singularity) — those get
/// live host FS views and don't need file sync.
pub struct FileSyncManager {
    get_files_fn: GetFilesFn,
    upload_fn: UploadFn,
    delete_fn: DeleteFn,
    bulk_upload_fn: BulkUploadFn,
    synced_files: HashMap<String, (f64, u64)>, // remote_path -> (mtime, size)
    last_sync_time: Instant,
    sync_interval: Duration,
}

impl FileSyncManager {
    /// Create a new file sync manager.
    pub fn new(
        get_files_fn: GetFilesFn,
        upload_fn: UploadFn,
        delete_fn: DeleteFn,
        bulk_upload_fn: BulkUploadFn,
    ) -> Self {
        // last_sync_time = Instant::now() ensures first sync runs immediately
        // (monotonic 0 would need Duration::MAX ago, but we use `elapsed()` check)
        Self {
            get_files_fn,
            upload_fn,
            delete_fn,
            bulk_upload_fn,
            synced_files: HashMap::new(),
            last_sync_time: Instant::now()
                .checked_sub(DEFAULT_SYNC_INTERVAL)
                .unwrap_or_else(Instant::now),
            sync_interval: DEFAULT_SYNC_INTERVAL,
        }
    }

    /// Create with a custom sync interval.
    pub fn with_sync_interval(mut self, interval: Duration) -> Self {
        self.sync_interval = interval;
        self
    }

    /// Run a sync cycle: upload changed files, delete removed files.
    ///
    /// Rate-limited to once per `sync_interval` unless `force` is true
    /// or `HERMES_FORCE_FILE_SYNC=1` is set.
    ///
    /// Transactional: state only committed if ALL operations succeed.
    /// On failure, state rolls back so the next cycle retries everything.
    pub fn sync(&mut self, force: bool) -> Result<(), String> {
        let force_sync = force || std::env::var(FORCE_SYNC_ENV).as_deref() == Ok("1");

        if !force_sync {
            let now = Instant::now();
            if now.duration_since(self.last_sync_time) < self.sync_interval {
                return Ok(());
            }
        }

        let current_files = (self.get_files_fn)();
        let current_remote_paths: std::collections::HashSet<&str> =
            current_files.iter().map(|(_, r)| r.as_str()).collect();

        // --- Uploads: new or changed files ---
        let mut to_upload: Vec<(String, String)> = Vec::new();
        let mut new_files = self.synced_files.clone();

        for (host_path, remote_path) in &current_files {
            if let Some(file_key) = file_mtime_key(host_path) {
                if self.synced_files.get(remote_path.as_str()) == Some(&file_key) {
                    continue;
                }
                to_upload.push((host_path.clone(), remote_path.clone()));
                new_files.insert(remote_path.clone(), file_key);
            }
        }

        // --- Deletes: synced paths no longer in current set ---
        let to_delete: Vec<String> = self
            .synced_files
            .keys()
            .filter(|p| !current_remote_paths.contains(p.as_str()))
            .cloned()
            .collect();

        if to_upload.is_empty() && to_delete.is_empty() {
            self.last_sync_time = Instant::now();
            return Ok(());
        }

        // Snapshot for rollback
        let prev_files = self.synced_files.clone();

        // Execute uploads
        let upload_result = if let (Some(bulk_fn), false) = (&self.bulk_upload_fn, to_upload.is_empty())
        {
            bulk_fn(&to_upload)
        } else {
            for (host, remote) in &to_upload {
                if let Err(e) = (self.upload_fn)(host, remote) {
                    return Err(format!("Upload failed for {host}: {e}"));
                }
            }
            Ok(())
        };

        if let Err(e) = upload_result {
            self.synced_files = prev_files;
            self.last_sync_time = Instant::now();
            return Err(e);
        }

        // Execute deletes
        if !to_delete.is_empty() {
            if let Err(e) = (self.delete_fn)(&to_delete) {
                self.synced_files = prev_files;
                self.last_sync_time = Instant::now();
                return Err(format!("Delete failed: {e}"));
            }
        }

        // --- Commit (all succeeded) ---
        for p in &to_delete {
            new_files.remove(p);
        }

        self.synced_files = new_files;
        self.last_sync_time = Instant::now();
        Ok(())
    }

    /// Force a sync regardless of rate limiting.
    pub fn sync_force(&mut self) -> Result<(), String> {
        self.sync(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quoted_rm_command() {
        let paths = vec!["/tmp/file1.txt".to_string(), "/tmp/file 2.txt".to_string()];
        let cmd = quoted_rm_command(&paths);
        assert!(cmd.starts_with("rm -f "));
        assert!(cmd.contains("/tmp/file 2.txt"));
    }

    #[test]
    fn test_quoted_mkdir_command() {
        let dirs = vec!["/tmp/a/b".to_string(), "/tmp/c/d".to_string()];
        let cmd = quoted_mkdir_command(&dirs);
        assert!(cmd.starts_with("mkdir -p "));
        assert!(cmd.contains("/tmp/a/b"));
        assert!(cmd.contains("/tmp/c/d"));
    }

    #[test]
    fn test_unique_parent_dirs() {
        let files = vec![
            ("/a/host1".to_string(), "/remote/dir1/file1.txt".to_string()),
            ("/a/host2".to_string(), "/remote/dir1/file2.txt".to_string()),
            ("/a/host3".to_string(), "/remote/dir2/file3.txt".to_string()),
        ];
        let dirs = unique_parent_dirs(&files);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], "/remote/dir1");
        assert_eq!(dirs[1], "/remote/dir2");
    }

    #[test]
    fn test_file_mtime_key_existing_file() {
        let tmp = std::env::temp_dir();
        let path = tmp.join("hermes_sync_test.txt");
        std::fs::write(&path, "hello").unwrap();
        let key = file_mtime_key(path.to_str().unwrap());
        assert!(key.is_some());
        let (_, size) = key.unwrap();
        assert_eq!(size, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_file_mtime_key_missing_file() {
        assert!(file_mtime_key("/nonexistent/path/file.txt").is_none());
    }

    #[test]
    fn test_sync_manager_no_changes() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = call_count.clone();

        let manager = FileSyncManager::new(
            Box::new(move || {
                count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![]
            }),
            Box::new(|_, _| Ok(())),
            Box::new(|_| Ok(())),
            None,
        );

        // First sync should run (no rate limit on first call)
        let mut manager = manager;
        assert!(manager.sync(false).is_ok());
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Immediate second sync should be rate-limited (get_files_fn NOT called)
        assert!(manager.sync(false).is_ok());
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_sync_force_bypasses_rate_limit() {
        let manager = FileSyncManager::new(
            Box::new(|| vec![]),
            Box::new(|_, _| Ok(())),
            Box::new(|_| Ok(())),
            None,
        );

        let mut manager = manager;
        assert!(manager.sync_force().is_ok());
        // Second force sync should also succeed
        assert!(manager.sync_force().is_ok());
    }

    #[test]
    fn test_send_sync() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<FileSyncManager>();
    }
}
