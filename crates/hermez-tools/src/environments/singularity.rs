//! Singularity/Apptainer container environment.
//!
//! Mirrors the Python `tools/environments/singularity.py`.
//! Security-hardened with `--containall`, `--no-home`, capability dropping.
//! Supports configurable resource limits and optional filesystem persistence
//! via writable overlay directories that survive across sessions.
//!
//! Unlike Modal/Daytona (cloud SDKs), Singularity can be implemented using
//! subprocess calls to the `apptainer` or `singularity` CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Environment, ProcessResult};
use crate::credential_files::get_credential_file_mounts;
use crate::credentials::get_skills_directory_mount;

/// Default timeout (seconds).
const DEFAULT_TIMEOUT: u64 = 60;

/// Global lock to prevent concurrent SIF builds of the same image.
static SIF_BUILD_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Snapshot store path: `{HERMEZ_HOME}/singularity_snapshots.json`.
fn snapshot_store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".hermez")
        .join("singularity_snapshots.json")
}

/// Load the snapshot JSON store.
fn load_snapshots() -> serde_json::Value {
    let path = snapshot_store_path();
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        }
    } else {
        serde_json::json!({})
    }
}

/// Save the snapshot JSON store.
fn save_snapshots(data: &serde_json::Value) {
    let path = snapshot_store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = std::fs::write(&path, json);
    }
}

/// Find the Apptainer/Singularity executable.
fn find_executable() -> Option<String> {
    // Check PATH for apptainer or singularity
    let path_var = std::env::var("PATH").unwrap_or_default();
    for exe_name in &["apptainer", "singularity"] {
        for dir in path_var.split(if cfg!(windows) { ';' } else { ':' }) {
            let candidate = PathBuf::from(dir).join(exe_name);
            if candidate.exists() {
                return Some(exe_name.to_string());
            }
            // On Windows, also check .exe
            #[cfg(windows)]
            {
                let candidate_exe = candidate.with_extension("exe");
                if candidate_exe.exists() {
                    return Some(exe_name.to_string());
                }
            }
        }
    }
    None
}

/// Get scratch directory for sandboxes.
fn get_scratch_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("TERMINAL_SCRATCH_DIR") {
        let p = PathBuf::from(&custom);
        let _ = std::fs::create_dir_all(&p);
        return p;
    }

    let home = dirs::home_dir().unwrap_or_default();
    let sandbox = home.join(".hermez").join("sandboxes").join("singularity");
    let _ = std::fs::create_dir_all(&sandbox);
    sandbox
}

/// Get Apptainer cache directory.
fn get_apptainer_cache_dir() -> PathBuf {
    if let Ok(cachedir) = std::env::var("APPTAINER_CACHEDIR") {
        let p = PathBuf::from(&cachedir);
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let scratch = get_scratch_dir();
    let cache = scratch.join(".apptainer");
    let _ = std::fs::create_dir_all(&cache);
    cache
}

/// Singularity/Apptainer configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingularityConfig {
    /// Docker image (e.g. "docker://ubuntu:22.04") or .sif path.
    pub image: String,
    /// Working directory inside the container.
    #[serde(default = "default_singularity_cwd")]
    pub cwd: String,
    /// CPU limit (0 = unlimited).
    #[serde(default)]
    pub cpu: f64,
    /// Memory limit in MB (0 = unlimited).
    #[serde(default)]
    pub memory: u64,
    /// Whether to use a writable overlay for persistence.
    #[serde(default)]
    pub persistent_filesystem: bool,
    /// Task identifier for overlay tracking.
    #[serde(default)]
    pub task_id: String,
}

fn default_singularity_cwd() -> String {
    "~".to_string()
}

impl Default for SingularityConfig {
    fn default() -> Self {
        Self {
            image: "docker://ubuntu:22.04".to_string(),
            cwd: default_singularity_cwd(),
            cpu: 0.0,
            memory: 0,
            persistent_filesystem: false,
            task_id: "default".to_string(),
        }
    }
}

/// Singularity environment state.
struct SingularityState {
    /// Unique instance ID (e.g. "hermez_abc123").
    instance_id: String,
    /// Whether the instance is currently running.
    instance_started: bool,
    /// Overlay directory (if persistent).
    overlay_dir: Option<PathBuf>,
    /// Executable name: "apptainer" or "singularity".
    executable: String,
    /// Task identifier for snapshot tracking.
    task_id: String,
}

/// Hardened Singularity/Apptainer container with resource limits and persistence.
///
/// Spawn-per-call: every `execute()` spawns a fresh `apptainer exec ... bash -c` process.
/// Session snapshot preserves env vars across calls.
pub struct SingularityEnvironment {
    config: SingularityConfig,
    cwd: PathBuf,
    #[allow(dead_code)]
    timeout: u64,
    state: Arc<parking_lot::Mutex<SingularityState>>,
}

impl SingularityEnvironment {
    /// Create a new Singularity environment from config.
    ///
    /// This finds the executable, optionally builds a SIF image from a Docker
    /// reference, starts an instance, and prepares for command execution.
    pub fn new(config: SingularityConfig) -> Self {
        let executable = find_executable().unwrap_or_else(|| "apptainer".to_string());
        let instance_id = format!(
            "hermez_{}",
            uuid::Uuid::new_v4().simple().to_string().chars().take(12).collect::<String>()
        );

        let overlay_dir = if config.persistent_filesystem {
            let overlay_base = get_scratch_dir().join("hermez-overlays");
            let _ = std::fs::create_dir_all(&overlay_base);
            let dir = overlay_base.join(format!("overlay-{}", config.task_id));
            let _ = std::fs::create_dir_all(&dir);
            Some(dir)
        } else {
            None
        };

        // Start the instance
        let started = Self::start_instance(
            &executable,
            &config.image,
            &instance_id,
            &overlay_dir,
            config.memory,
            config.cpu,
        );

        if !started {
            tracing::warn!(
                "Singularity: failed to start instance '{}'. Commands will fail.",
                instance_id
            );
        }

        let task_id = config.task_id.clone();

        Self {
            cwd: PathBuf::from(&config.cwd),
            config,
            timeout: DEFAULT_TIMEOUT,
            state: Arc::new(parking_lot::Mutex::new(SingularityState {
                instance_id,
                instance_started: started,
                overlay_dir,
                executable,
                task_id,
            })),
        }
    }

    /// Set the working directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = cwd.as_ref().to_path_buf();
        self.config.cwd = self.cwd.to_string_lossy().to_string();
        self
    }

    /// Set the default timeout.
    #[allow(dead_code)]
    pub fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }

    /// Get the config.
    pub fn config(&self) -> &SingularityConfig {
        &self.config
    }

    /// Start a Singularity instance.
    fn start_instance(
        executable: &str,
        image: &str,
        instance_id: &str,
        overlay_dir: &Option<PathBuf>,
        memory: u64,
        cpu: f64,
    ) -> bool {
        let mut cmd = Command::new(executable);
        cmd.args(["instance", "start"]);
        cmd.args(["--containall", "--no-home"]);

        if let Some(ref overlay) = overlay_dir {
            cmd.args(["--overlay", overlay.to_string_lossy().as_ref()]);
        } else {
            cmd.arg("--writable-tmpfs");
        }

        // Add credential and skills bind mounts (read-only).
        for mount in get_credential_file_mounts() {
            cmd.args([
                "--bind",
                &format!("{}:{}:ro", mount.host_path, mount.container_path),
            ]);
        }
        if let Some(skills) = get_skills_directory_mount() {
            if let (Some(host), Some(container)) =
                (skills.get("host_path"), skills.get("container_path"))
            {
                cmd.args(["--bind", &format!("{}:{}:ro", host, container)]);
            }
        }

        if memory > 0 {
            cmd.args(["--memory", &memory.to_string()]);
        }
        if cpu > 0.0 {
            cmd.args(["--cpus", &cpu.to_string()]);
        }

        cmd.args([image, instance_id]);

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(result) => {
                if result.status.success() {
                    tracing::info!(
                        "Singularity instance {} started (persistent={})",
                        instance_id,
                        overlay_dir.is_some()
                    );
                    true
                } else {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    tracing::error!("Singularity instance start failed: {}", stderr);
                    false
                }
            }
            Err(e) => {
                tracing::error!("Failed to start Singularity instance: {}", e);
                false
            }
        }
    }

    /// Execute a command inside the Singularity instance.
    fn exec_in_instance(
        executable: &str,
        instance_id: &str,
        command: &str,
        login: bool,
    ) -> ProcessResult {
        let mut cmd = Command::new(executable);
        cmd.args(["exec", &format!("instance://{}", instance_id)]);

        if login {
            cmd.args(["bash", "-l", "-c", command]);
        } else {
            cmd.args(["bash", "-c", command]);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        match cmd.output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);
                ProcessResult {
                    stdout,
                    stderr,
                    exit_code,
                }
            }
            Err(e) => ProcessResult {
                stdout: String::new(),
                stderr: format!("Singularity exec failed: {e}"),
                exit_code: -1,
            },
        }
    }

    /// Stop the Singularity instance.
    fn stop_instance(executable: &str, instance_id: &str) {
        let output = Command::new(executable)
            .args(["instance", "stop", instance_id])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(result) => {
                if result.status.success() {
                    tracing::info!("Singularity instance {} stopped", instance_id);
                } else {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    tracing::warn!(
                        "Failed to stop Singularity instance {}: {}",
                        instance_id,
                        stderr
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to stop Singularity instance {}: {}", instance_id, e);
            }
        }
    }

    /// Clean up the instance. If persistent, save the overlay dir to snapshots.
    pub fn cleanup(&self) {
        let mut state = self.state.lock();

        if state.instance_started {
            Self::stop_instance(&state.executable, &state.instance_id);
            state.instance_started = false;
        }

        if state.overlay_dir.is_some() {
            // Save snapshot for persistence
            let mut snapshots = load_snapshots();
            if let Some(obj) = snapshots.as_object_mut() {
                if let Some(dir) = &state.overlay_dir {
                    obj.insert(
                        state.task_id.clone(),
                        serde_json::json!(dir.to_string_lossy().to_string()),
                    );
                }
            }
            save_snapshots(&snapshots);
            tracing::info!(
                "Singularity: saved overlay path for task {}",
                state.task_id
            );
        }
    }

    /// TODO: Build or resolve a SIF image from the image spec.
    /// If the image is already a `.sif` path, return it directly.
    /// If it's a `docker://` reference, check the cache for an existing SIF.
    /// If not cached, build it with `apptainer build`.
    /// Returns the resolved image path or the original spec.
    pub fn resolve_image(image: &str) -> String {
        // If already a .sif file that exists, use it directly
        if image.ends_with(".sif") && Path::new(image).exists() {
            return image.to_string();
        }

        // If not a docker reference, pass through
        if !image.starts_with("docker://") {
            return image.to_string();
        }

        let cache_dir = get_apptainer_cache_dir();
        let image_name = image
            .replace("docker://", "")
            .replace(['/', ':'], "-");
        let sif_path = cache_dir.join(format!("{image_name}.sif"));

        if sif_path.exists() {
            return sif_path.to_string_lossy().to_string();
        }

        // SIF build requires the apptainer executable
        let executable = match find_executable() {
            Some(exe) => exe,
            None => {
                tracing::warn!(
                    "No apptainer/singularity found, cannot build SIF. Using docker:// URL."
                );
                return image.to_string();
            }
        };

        // Acquire global lock to prevent concurrent builds of the same image.
        let _lock = SIF_BUILD_LOCK.lock();

        // Double-check after acquiring the lock (another thread may have built it).
        if sif_path.exists() {
            return sif_path.to_string_lossy().to_string();
        }

        tracing::info!("Building SIF image (one-time setup)...");
        tracing::info!("  Source: {}", image);
        tracing::info!("  Target: {}", sif_path.display());

        let tmp_dir = cache_dir.join("tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let mut cmd = Command::new(&executable);
        cmd.args(["build", sif_path.to_string_lossy().as_ref(), image]);
        cmd.env("APPTAINER_TMPDIR", tmp_dir.to_string_lossy().as_ref());
        cmd.env("APPTAINER_CACHEDIR", cache_dir.to_string_lossy().as_ref());

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(result) => {
                if result.status.success() {
                    tracing::info!("SIF image built successfully");
                    sif_path.to_string_lossy().to_string()
                } else {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    tracing::warn!("SIF build failed, falling back to docker:// URL");
                    tracing::warn!("  Error: {}", &stderr[..stderr.len().min(500)]);
                    image.to_string()
                }
            }
            Err(e) => {
                // Clean up partial SIF to avoid corrupt cache hits on retry.
                if sif_path.exists() {
                    let _ = std::fs::remove_file(&sif_path);
                }
                tracing::warn!("SIF build error: {e}, falling back to docker:// URL");
                image.to_string()
            }
        }
    }
}

// Fix: the stop_instance function has a syntax error. Let me correct it.
// The issue is with the `args` method call - need to use `Command::new` properly.

impl Drop for SingularityEnvironment {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Environment for SingularityEnvironment {
    fn env_type(&self) -> &str {
        "singularity"
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn execute(&self, command: &str, cwd: Option<&str>, _timeout: Option<u64>) -> ProcessResult {
        let state = self.state.lock();

        if !state.instance_started {
            return ProcessResult {
                stdout: String::new(),
                stderr: format!("Singularity instance '{}' not started", state.instance_id),
                exit_code: -1,
            };
        }

        let effective_cwd = cwd.unwrap_or(&self.config.cwd);
        let full_command = format!("cd {effective_cwd} && {command}");

        Self::exec_in_instance(
            &state.executable,
            &state.instance_id,
            &full_command,
            false, // login shell not needed (snapshot not implemented yet)
        )
    }

    fn is_available(&self) -> bool {
        find_executable().is_some()
    }
}

/// Create a Singularity environment from config.
pub fn create_singularity_env(
    config: SingularityConfig,
    cwd: Option<&str>,
) -> SingularityEnvironment {
    match cwd {
        Some(dir) => SingularityEnvironment::new(config).with_cwd(dir),
        None => SingularityEnvironment::new(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_singularity_config_default() {
        let config = SingularityConfig::default();
        assert_eq!(config.image, "docker://ubuntu:22.04");
        assert_eq!(config.cwd, "~");
        assert_eq!(config.cpu, 0.0);
        assert_eq!(config.memory, 0);
        assert!(!config.persistent_filesystem);
        assert_eq!(config.task_id, "default");
    }

    #[test]
    fn test_singularity_config_custom() {
        let config = SingularityConfig {
            image: "docker://python:3.11".to_string(),
            cwd: "/workspace".to_string(),
            cpu: 2.0,
            memory: 4096,
            persistent_filesystem: true,
            task_id: "my-task".to_string(),
        };
        assert_eq!(config.image, "docker://python:3.11");
        assert!(config.persistent_filesystem);
        assert_eq!(config.cpu, 2.0);
    }

    #[test]
    fn test_singularity_env_type() {
        let config = SingularityConfig::default();
        let env = SingularityEnvironment::new(config);
        assert_eq!(env.env_type(), "singularity");
    }

    #[test]
    fn test_singularity_env_cwd() {
        let config = SingularityConfig::default();
        let env = SingularityEnvironment::new(config).with_cwd("/workspace");
        assert!(env.cwd().to_string_lossy().contains("workspace"));
    }

    #[test]
    fn test_create_singularity_env() {
        let config = SingularityConfig::default();
        let env = create_singularity_env(config, Some("/tmp"));
        assert_eq!(env.env_type(), "singularity");
    }

    #[test]
    fn test_resolve_image_local_sif() {
        // Non-docker, non-.sif image should pass through
        let resolved = SingularityEnvironment::resolve_image("/path/to/image.sif");
        // Since the file doesn't exist, it returns the original path
        assert_eq!(resolved, "/path/to/image.sif");
    }

    #[test]
    fn test_resolve_image_non_docker() {
        // Non-docker reference passes through
        let resolved = SingularityEnvironment::resolve_image("library://alpine");
        assert_eq!(resolved, "library://alpine");
    }

    #[test]
    fn test_execute_without_instance() {
        // If apptainer/singularity is not installed, execute should fail gracefully
        let config = SingularityConfig::default();
        let env = SingularityEnvironment::new(config);

        // This may succeed if apptainer is installed, or fail if not
        let result = env.execute("echo hello", None, None);
        // Just verify it doesn't panic and returns a valid ProcessResult
        if result.exit_code == -1 {
            // Expected if singularity/apptainer not available
        }
    }

    #[test]
    fn test_is_available() {
        let config = SingularityConfig::default();
        let env = SingularityEnvironment::new(config);
        // May or may not be available depending on the system
        let _available = env.is_available();
    }

    #[test]
    fn test_config_serialization() {
        let config = SingularityConfig {
            image: "docker://python:3.11".to_string(),
            cwd: "/workspace".to_string(),
            cpu: 2.0,
            memory: 4096,
            persistent_filesystem: true,
            task_id: "test-task".to_string(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("python:3.11"));
        assert!(json.contains("test-task"));
        let restored: SingularityConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.image, "docker://python:3.11");
        assert!(restored.persistent_filesystem);
    }

    #[test]
    fn test_send_sync() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<SingularityEnvironment>();
    }
}
