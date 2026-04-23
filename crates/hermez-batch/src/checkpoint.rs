//! Batch processing checkpoint — atomic save/load, content-based resume.
//!
//! Mirrors the Python `_load_checkpoint` and `_save_checkpoint` in `batch_runner.py`.

use std::path::Path;

use chrono;
use serde::{Deserialize, Serialize};

use hermez_core::{HermezError, Result};

/// Persistent checkpoint state for batch processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Name of the batch run.
    pub run_name: String,
    /// Set of completed prompt texts (content-based tracking).
    pub completed_prompts: Vec<String>,
    /// Per-batch statistics.
    pub batch_stats: Vec<BatchStat>,
    /// Last checkpoint update time (Unix epoch).
    pub last_updated: u64,
}

/// Statistics for a single batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchStat {
    pub batch_num: usize,
    pub processed: usize,
    pub skipped: usize,
    pub total_api_calls: usize,
    pub total_tokens_input: usize,
    pub total_tokens_output: usize,
}

impl Checkpoint {
    /// Create a new empty checkpoint.
    pub fn new(run_name: &str) -> Self {
        Self {
            run_name: run_name.to_string(),
            completed_prompts: Vec::new(),
            batch_stats: Vec::new(),
            last_updated: chrono::Utc::now().timestamp() as u64,
        }
    }

    /// Load checkpoint from file.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(path).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to read checkpoint file: {e}"),
            )
        })?;
        let cp: Checkpoint = serde_json::from_str(&data).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to parse checkpoint file: {e}"),
            )
        })?;
        Ok(Some(cp))
    }

    /// Save checkpoint atomically (write to temp, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string_pretty(self).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to serialize checkpoint: {e}"),
            )
        })?;

        // Atomic write: write to temp file, then rename
        let temp_path = path.with_extension("tmp");
        std::fs::write(&temp_path, &data).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to write checkpoint: {e}"),
            )
        })?;

        std::fs::rename(&temp_path, path).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to rename checkpoint: {e}"),
            )
        })
    }

    /// Record a completed prompt and save.
    pub fn record_completion(&mut self, prompt: &str, path: &Path) -> Result<()> {
        self.completed_prompts.push(prompt.to_string());
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        self.save(path)
    }

    /// Record batch statistics and save.
    pub fn record_batch(&mut self, stat: BatchStat, path: &Path) -> Result<()> {
        self.batch_stats.push(stat);
        self.last_updated = chrono::Utc::now().timestamp() as u64;
        self.save(path)
    }

    /// Check if a prompt has already been completed.
    pub fn is_completed(&self, prompt: &str) -> bool {
        self.completed_prompts.iter().any(|p| p == prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_save_and_load() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint.json");

        let mut cp = Checkpoint::new("test_run");
        cp.completed_prompts.push("hello world".to_string());
        cp.batch_stats.push(BatchStat {
            batch_num: 0,
            processed: 5,
            skipped: 0,
            total_api_calls: 10,
            total_tokens_input: 1000,
            total_tokens_output: 500,
        });
        cp.save(&path).unwrap();

        let loaded = Checkpoint::load(&path).unwrap().unwrap();
        assert_eq!(loaded.run_name, "test_run");
        assert_eq!(loaded.completed_prompts.len(), 1);
        assert_eq!(loaded.batch_stats.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_checkpoint_nonexistent() {
        let result = Checkpoint::load(Path::new("/tmp/nonexistent_checkpoint_xyz.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_checkpoint_record_completion() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint2.json");

        let mut cp = Checkpoint::new("run");
        assert!(!cp.is_completed("test prompt"));
        cp.record_completion("test prompt", &path).unwrap();
        assert!(cp.is_completed("test prompt"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_checkpoint_record_batch() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint_batch.json");

        let mut cp = Checkpoint::new("batch_run");
        cp.record_batch(BatchStat {
            batch_num: 0,
            processed: 10,
            skipped: 2,
            total_api_calls: 30,
            total_tokens_input: 5000,
            total_tokens_output: 2500,
        }, &path).unwrap();

        assert_eq!(cp.batch_stats.len(), 1);
        assert_eq!(cp.batch_stats[0].batch_num, 0);
        assert_eq!(cp.batch_stats[0].processed, 10);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_checkpoint_multiple_completions() {
        let mut cp = Checkpoint::new("multi");
        cp.record_completion("prompt 1", &std::env::temp_dir().join("test_multi.json")).unwrap();
        cp.record_completion("prompt 2", &std::env::temp_dir().join("test_multi.json")).unwrap();
        cp.record_completion("prompt 3", &std::env::temp_dir().join("test_multi.json")).unwrap();

        assert!(cp.is_completed("prompt 1"));
        assert!(cp.is_completed("prompt 2"));
        assert!(cp.is_completed("prompt 3"));
        assert!(!cp.is_completed("prompt 4"));
        assert_eq!(cp.completed_prompts.len(), 3);

        let _ = std::fs::remove_file(&std::env::temp_dir().join("test_multi.json"));
    }

    #[test]
    fn test_checkpoint_atomic_write() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint_atomic.json");

        let cp = Checkpoint::new("atomic_test");
        cp.save(&path).unwrap();

        // File should exist, temp file should not
        assert!(path.exists());
        let temp_path = path.with_extension("tmp");
        assert!(!temp_path.exists(), "Temp file should have been renamed");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_checkpoint_update_timestamp() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint_time.json");

        let mut cp = Checkpoint::new("time_test");
        let ts1 = cp.last_updated;

        // Wait a bit and record
        std::thread::sleep(std::time::Duration::from_secs(1));
        cp.record_completion("test", &path).unwrap();

        assert!(cp.last_updated >= ts1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_checkpoint_corrupted_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_checkpoint_corrupt.json");

        std::fs::write(&path, "this is not json{{{{").unwrap();
        let result = Checkpoint::load(&path);
        assert!(result.is_err(), "Should fail to parse corrupted JSON");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_batch_stat_fields() {
        let stat = BatchStat {
            batch_num: 5,
            processed: 8,
            skipped: 2,
            total_api_calls: 24,
            total_tokens_input: 12000,
            total_tokens_output: 6000,
        };
        assert_eq!(stat.batch_num, 5);
        assert_eq!(stat.processed + stat.skipped, 10);
    }
}
