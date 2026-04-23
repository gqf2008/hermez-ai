//! Modal cloud execution environment.
//!
//! Mirrors the Python `tools/environments/modal.py`.
//! Uses Modal sandboxes for serverless cloud execution.
//! Supports persistent snapshots: filesystem state is captured on cleanup
//! and restored on next creation, preserving work across sessions.
//!
//! **SKELETON**: Full implementation requires the Modal SDK integration.
//! Modal's Python SDK (`modal`) uses gRPC under the hood and provides:
//! - `modal.App.lookup()` — get or create an app
//! - `modal.Sandbox.create()` — create a sandbox container
//! - `sandbox.exec()` — execute commands inside the sandbox
//! - `sandbox.snapshot_filesystem()` — capture filesystem as image
//! - `modal.Mount` — mount local files into the sandbox
//!
//! A Rust implementation would need either:
//! 1. A Rust gRPC client implementing Modal's internal protocol, or
//! 2. FFI calls to the Python SDK via pyo3.
//!
//! Key considerations:
//! - Modal uses async operations; the Rust impl would need an async runtime.
//! - File uploads use base64-encoded stdin streams or tar archives.
//! - Snapshot persistence stores image IDs in a JSON file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Environment, ProcessResult};

/// Modal sandbox configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModalConfig {
    /// Docker image or Modal image ID (prefix "im-").
    pub image: String,
    /// Working directory inside the sandbox.
    #[serde(default = "default_modal_cwd")]
    pub cwd: String,
    /// Whether to preserve the filesystem across sessions via snapshots.
    #[serde(default = "default_persistent")]
    pub persistent_filesystem: bool,
    /// Task identifier for snapshot tracking.
    #[serde(default)]
    pub task_id: String,
    /// Additional sandbox parameters (passed through to Modal SDK).
    /// TODO: Define concrete fields when SDK integration is available.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extra_params: std::collections::HashMap<String, serde_json::Value>,
}

fn default_modal_cwd() -> String {
    "/root".to_string()
}

fn default_persistent() -> bool {
    true
}

impl Default for ModalConfig {
    fn default() -> Self {
        Self {
            image: "ubuntu:22.04".to_string(),
            cwd: default_modal_cwd(),
            persistent_filesystem: default_persistent(),
            task_id: "default".to_string(),
            extra_params: std::collections::HashMap::new(),
        }
    }
}

/// Modal cloud execution via native Modal sandboxes.
///
/// **SKELETON**: The actual sandbox lifecycle (create/snapshot/terminate)
/// and command execution require Modal SDK integration.
pub struct ModalEnvironment {
    config: ModalConfig,
    cwd: PathBuf,
    /// Whether the sandbox has been initialized.
    /// TODO: Replace with actual sandbox state (sandbox handle, app reference).
    #[allow(dead_code)]
    initialized: Arc<parking_lot::Mutex<bool>>,
    /// Path to the snapshot store JSON file.
    /// TODO: Use `hermez_constants::get_hermez_home() / "modal_snapshots.json"`.
    #[allow(dead_code)]
    snapshot_path: PathBuf,
}

impl ModalEnvironment {
    /// Create a new Modal environment from config.
    ///
    /// **TODO**: Implement sandbox lifecycle:
    /// 1. Check `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` env vars for auth.
    /// 2. If `persistent_filesystem` is true, load snapshot from
    ///    `{HERMEZ_HOME}/modal_snapshots.json` under key `"direct:{task_id}"`.
    /// 3. Resolve image: if it starts with "im-", use `Image.from_id()`.
    ///    Otherwise, use `Image.from_registry()` with optional python setup
    ///    for ubuntu/debian images.
    /// 4. Create credential mounts from `credential_files` module.
    /// 5. Create sandbox: `await modal.Sandbox.create("sleep", "infinity", ...)`
    /// 6. If persistent restore fails, fall back to base image.
    /// 7. Initialize `FileSyncManager` for credential/skill/cache sync.
    ///    Upload files via base64 stdin pipe (chunked at 1MB).
    /// 8. Capture env snapshot (init_session equivalent).
    pub fn new(config: ModalConfig) -> Self {
        let snapshot_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".hermez")
            .join("modal_snapshots.json");

        tracing::warn!(
            "ModalEnvironment is a skeleton — full SDK integration required. \
             Set MODAL_TOKEN_ID and MODAL_TOKEN_SECRET env vars."
        );

        Self {
            cwd: PathBuf::from(&config.cwd),
            config,
            initialized: Arc::new(parking_lot::Mutex::new(false)),
            snapshot_path,
        }
    }

    /// Set the working directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = cwd.as_ref().to_path_buf();
        self.config.cwd = self.cwd.to_string_lossy().to_string();
        self
    }

    /// Get the config.
    pub fn config(&self) -> &ModalConfig {
        &self.config
    }

    /// TODO: Load a previously saved snapshot ID for this task.
    /// Returns `(snapshot_id, was_legacy_key)`.
    fn _load_snapshot(&self, _task_id: &str) -> Option<(String, bool)> {
        // TODO: Read from `{HERMEZ_HOME}/modal_snapshots.json`.
        // Look for key "direct:{task_id}" first, then fall back to legacy "{task_id}".
        // Python equivalent: _get_snapshot_restore_candidate()
        None
    }

    /// TODO: Save a snapshot ID for this task.
    fn _save_snapshot(&self, _task_id: &str, _snapshot_id: &str) {
        // TODO: Write to `{HERMEZ_HOME}/modal_snapshots.json`.
        // Key: "direct:{task_id}", value: snapshot_id (image object_id).
        // Python equivalent: _store_direct_snapshot()
    }

    /// TODO: Delete a snapshot entry for this task.
    fn _delete_snapshot(&self, _task_id: &str) {
        // TODO: Remove keys "direct:{task_id}" and "{task_id}" from snapshot store.
        // Python equivalent: _delete_direct_snapshot()
    }

    /// TODO: Resolve an image spec string into a Modal image.
    /// - If starts with "im-": `Image.from_id(spec)`
    /// - Otherwise: `Image.from_registry(spec, setup_dockerfile_commands=[...])`
    ///   For ubuntu/debian, add python installation commands.
    fn _resolve_image(&self, _image_spec: &str) -> Result<(), String> {
        // TODO: Implement image resolution.
        // For ubuntu/debian images, inject:
        //   RUN apt-get update -qq && apt-get install -y -qq python3 python3-venv
        //   RUN rm -rf /usr/local/lib/python*/site-packages/pip* 2>/dev/null; python -m ensurepip --upgrade --default-pip 2>/dev/null || true
        Ok(())
    }

    /// TODO: Upload a single file to the sandbox.
    /// Uses base64 encoding piped through stdin:
    /// `mkdir -p {dir} && base64 -d > {remote_path}`
    /// Chunk size: 1MB per write to stay under SDK buffer limits.
    fn _upload_file(&self, _host_path: &str, _remote_path: &str) -> Result<(), String> {
        // TODO: Read file, base64-encode, stream through sandbox exec stdin.
        // Python equivalent: _modal_upload()
        Err("Modal: file upload not implemented — SDK integration required".to_string())
    }

    /// TODO: Bulk upload files via tar archive piped through stdin.
    /// Builds a gzipped tar archive, base64-encodes it, and streams it into:
    /// `mkdir -p ... && base64 -d | tar xzf - -C /`
    fn _bulk_upload(&self, _files: &[(String, String)]) -> Result<(), String> {
        // TODO: Build tar.gz archive, base64-encode, stream through stdin.
        // Python equivalent: _modal_bulk_upload()
        Err("Modal: bulk upload not implemented — SDK integration required".to_string())
    }

    /// TODO: Delete files in the sandbox.
    /// Executes `rm -f {paths}` via sandbox exec.
    fn _delete_files(&self, _remote_paths: &[String]) -> Result<(), String> {
        // TODO: Execute rm command via sandbox exec.
        // Python equivalent: _modal_delete()
        Err("Modal: file delete not implemented — SDK integration required".to_string())
    }

    /// TODO: Execute a command inside the sandbox.
    /// Uses `sandbox.exec("bash", "-c", command, timeout=timeout)`.
    /// Reads stdout and stderr, returns combined output and exit code.
    fn _exec_in_sandbox(&self, _command: &str, _timeout: u64) -> ProcessResult {
        // TODO: Implement via Modal sandbox exec API.
        // Python equivalent (async):
        //   process = await sandbox.exec.aio("bash", "-c", cmd_string, timeout=timeout)
        //   stdout = await process.stdout.read.aio()
        //   exit_code = await process.wait.aio()
        ProcessResult {
            stdout: String::new(),
            stderr: "Modal: command execution not implemented — SDK integration required".to_string(),
            exit_code: -1,
        }
    }

    /// TODO: Clean up the sandbox.
    /// If persistent: capture filesystem snapshot, save ID, then terminate.
    /// Otherwise: just terminate.
    /// Python equivalent:
    ///   if persistent:
    ///       img = await sandbox.snapshot_filesystem.aio()
    ///       _store_direct_snapshot(task_id, img.object_id)
    ///   await sandbox.terminate.aio()
    pub fn cleanup(&self) {
        // TODO: Snapshot filesystem if persistent, then terminate sandbox.
        tracing::warn!("ModalEnvironment: cleanup is a no-op (skeleton)");
    }
}

impl Environment for ModalEnvironment {
    fn env_type(&self) -> &str {
        "modal"
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn execute(&self, command: &str, cwd: Option<&str>, timeout: Option<u64>) -> ProcessResult {
        let effective_cwd = cwd.unwrap_or_else(|| &self.config.cwd);
        let effective_timeout = timeout.unwrap_or(60);

        // TODO: Before execution:
        // 1. Sync files via FileSyncManager (rate-limited)
        // 2. Build wrapped command with CWD change and env snapshot sourcing
        //    (see Python BaseEnvironment._wrap_command)
        //    This includes: source snapshot, cd, eval command, re-dump env,
        //    emit CWD markers.

        let _full_command = format!("cd {effective_cwd} && {command}");

        self._exec_in_sandbox(command, effective_timeout)
    }

    fn is_available(&self) -> bool {
        // TODO: Check Modal API connectivity.
        // Python equivalent: try importing `modal` and creating an App.
        std::env::var("MODAL_TOKEN_ID").is_ok()
            && std::env::var("MODAL_TOKEN_SECRET").is_ok()
    }
}

impl Drop for ModalEnvironment {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Create a Modal environment from config.
pub fn create_modal_env(config: ModalConfig, cwd: Option<&str>) -> ModalEnvironment {
    match cwd {
        Some(dir) => ModalEnvironment::new(config).with_cwd(dir),
        None => ModalEnvironment::new(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modal_config_default() {
        let config = ModalConfig::default();
        assert_eq!(config.image, "ubuntu:22.04");
        assert_eq!(config.cwd, "/root");
        assert!(config.persistent_filesystem);
        assert_eq!(config.task_id, "default");
        assert!(config.extra_params.is_empty());
    }

    #[test]
    fn test_modal_config_custom() {
        let config = ModalConfig {
            image: "im-abc123".to_string(),
            cwd: "/workspace".to_string(),
            persistent_filesystem: false,
            task_id: "my-task".to_string(),
            extra_params: std::collections::HashMap::new(),
        };
        assert_eq!(config.image, "im-abc123");
        assert!(!config.persistent_filesystem);
    }

    #[test]
    fn test_modal_env_type() {
        let config = ModalConfig::default();
        let env = ModalEnvironment::new(config);
        assert_eq!(env.env_type(), "modal");
    }

    #[test]
    fn test_modal_env_cwd() {
        let config = ModalConfig::default();
        let env = ModalEnvironment::new(config).with_cwd("/workspace");
        assert!(env.cwd().to_string_lossy().contains("workspace"));
    }

    #[test]
    fn test_create_modal_env() {
        let config = ModalConfig::default();
        let env = create_modal_env(config, Some("/tmp"));
        assert_eq!(env.env_type(), "modal");
    }

    #[test]
    fn test_execute_returns_skeleton_error() {
        let config = ModalConfig::default();
        let env = ModalEnvironment::new(config);
        let result = env.execute("echo hello", None, None);
        assert_eq!(result.exit_code, -1);
        assert!(result.stderr.contains("skeleton") || result.stderr.contains("not implemented"));
    }

    #[test]
    fn test_is_available_without_env() {
        let config = ModalConfig::default();
        let env = ModalEnvironment::new(config);
        // Without env vars, should return false
        assert!(!env.is_available());
    }

    #[test]
    fn test_send_sync() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<ModalEnvironment>();
    }

    #[test]
    fn test_config_serialization() {
        let config = ModalConfig {
            image: "python:3.11".to_string(),
            cwd: "/workspace".to_string(),
            persistent_filesystem: false,
            task_id: "test-task".to_string(),
            extra_params: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("python:3.11"));
        assert!(json.contains("test-task"));
        let restored: ModalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.image, "python:3.11");
    }
}
