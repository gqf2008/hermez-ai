//! Docker terminal environment backend.
//!
//! Mirrors the Python `tools/environments/docker.py`.
//! Uses `bollard` to interact with the Docker Engine API.
//! Supports bind mounts, CPU/memory limits, and container reuse.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bollard::container::{Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions};
use bollard::exec::CreateExecOptions;
use bollard::models::{ContainerSummary, HostConfig, Mount, MountTypeEnum};
use bollard::Docker;
use futures_util::StreamExt;
use tokio::runtime::Handle;

use super::{Environment, ProcessResult};

/// Default Docker image if none specified.
const DEFAULT_IMAGE: &str = "ubuntu:22.04";

/// Default container timeout (seconds).
#[allow(dead_code)]
const DEFAULT_TIMEOUT: u64 = 60;

/// Shared Docker connection and container state.
struct DockerState {
    container_id: Option<String>,
}

/// Docker environment configuration.
#[derive(Debug, Clone)]
pub struct DockerConfig {
    /// Docker image to use.
    pub image: String,
    /// Bind mounts: (host_path, container_path).
    pub bind_mounts: Vec<(String, String)>,
    /// CPU limit (fraction, e.g. 0.5 = 50%).
    pub cpu_limit: Option<f64>,
    /// Memory limit in bytes.
    pub memory_limit: Option<u64>,
    /// Working directory inside the container.
    pub workdir: Option<String>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: DEFAULT_IMAGE.to_string(),
            bind_mounts: Vec::new(),
            cpu_limit: None,
            memory_limit: None,
            workdir: None,
        }
    }
}

/// Docker environment — executes commands inside a Docker container.
pub struct DockerEnvironment {
    config: DockerConfig,
    cwd: PathBuf,
    #[allow(dead_code)]
    timeout: u64,
    state: Arc<parking_lot::Mutex<DockerState>>,
}

impl DockerEnvironment {
    /// Create a new Docker environment.
    pub fn new(config: DockerConfig) -> Self {
        Self {
            cwd: PathBuf::from("."),
            config,
            timeout: DEFAULT_TIMEOUT,
            state: Arc::new(parking_lot::Mutex::new(DockerState {
                container_id: None,
            })),
        }
    }

    /// Set the working directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = cwd.as_ref().to_path_buf();
        self
    }

    /// Set the default timeout.
    #[allow(dead_code)]
    pub fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }

    /// Get the config.
    #[allow(dead_code)]
    pub fn config(&self) -> &DockerConfig {
        &self.config
    }

    /// Bridge: run an async closure synchronously using the current tokio runtime
    /// or creating a new one if none exists.
    fn block_on<F, T>(f: F) -> Result<T, String>
    where
        F: std::future::Future<Output = Result<T, String>>,
    {
        match Handle::try_current() {
            Ok(handle) => handle.block_on(f),
            Err(_) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| format!("Failed to create runtime: {e}"))?;
                rt.block_on(f)
            }
        }
    }

    /// Get or create the container.
    fn get_or_create_container(&self) -> Result<String, String> {
        let state_clone = self.state.clone();
        let config = self.config.clone();
        let cwd = self.cwd.clone();

        Self::block_on(async move {
            let docker = Docker::connect_with_local_defaults()
                .map_err(|e| format!("Docker not available: {e}"))?;

            // Check if existing container is still running
            let existing_cid = {
                let state = state_clone.lock();
                state.container_id.clone()
            };
            if let Some(ref cid) = existing_cid {
                match docker.inspect_container(cid, None).await {
                    Ok(info) => {
                        if info.state.as_ref().and_then(|s| s.running) == Some(true) {
                            return Ok(cid.clone());
                        }
                    }
                    Err(_) => {
                        let mut state = state_clone.lock();
                        state.container_id = None;
                    }
                }
            }

            // Build host config
            let mounts: Vec<Mount> = config.bind_mounts.iter().map(|(host, container)| Mount {
                target: Some(container.clone()),
                source: Some(host.clone()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(false),
                ..Default::default()
            }).collect();

            let mut host_config = HostConfig {
                mounts: Some(mounts),
                ..Default::default()
            };

            if let Some(cpu) = config.cpu_limit {
                host_config.nano_cpus = Some((cpu * 1_000_000_000.0) as i64);
            }
            if let Some(mem) = config.memory_limit {
                host_config.memory = Some(mem as i64);
            }

            let workdir = config.workdir.clone().or_else(|| Some(cwd.to_string_lossy().to_string()));

            let container_config = Config {
                image: Some(config.image.clone()),
                host_config: Some(host_config),
                working_dir: workdir,
                tty: Some(false),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec!["bash".to_string(), "-c".to_string(), "while true; do sleep 86400; done".to_string()]),
                ..Default::default()
            };

            let label = format!("hermez-agent-{}", std::process::id());
            let options = CreateContainerOptions {
                name: label,
                platform: None,
            };

            let created = docker.create_container(Some(options), container_config)
                .await
                .map_err(|e| format!("Failed to create container: {e}"))?;

            let cid = created.id;
            docker.start_container::<&str>(&cid, None)
                .await
                .map_err(|e| format!("Failed to start container: {e}"))?;

            state_clone.lock().container_id = Some(cid.clone());
            Ok(cid)
        })
    }

    /// Execute a command inside the container.
    fn exec_in_container(&self, command: &str) -> ProcessResult {
        // Get container ID (creates if needed)
        let container_id = {
            let state = self.state.lock();
            state.container_id.clone()
        };

        let container_id = match container_id {
            Some(id) => id,
            None => {
                match self.get_or_create_container() {
                    Ok(cid) => cid,
                    Err(e) => return ProcessResult {
                        stdout: String::new(),
                        stderr: e,
                        exit_code: -1,
                    },
                }
            }
        };

        let container_id_clone = container_id.clone();
        let command = command.to_string();

        match Self::block_on(async move {
            let docker = Docker::connect_with_local_defaults()
                .map_err(|e| format!("Docker not available: {e}"))?;

            let exec_config = CreateExecOptions {
                cmd: Some(vec!["bash", "-lc", &command]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            };

            let exec = docker.create_exec(&container_id_clone, exec_config)
                .await
                .map_err(|e| format!("Failed to create exec: {e}"))?;

            match docker.start_exec(&exec.id, None).await {
                Ok(bollard::exec::StartExecResults::Attached { mut output, .. }) => {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    while let Some(item) = output.next().await {
                        match item {
                            Ok(bollard::container::LogOutput::StdOut { message }) => {
                                stdout.push_str(&String::from_utf8_lossy(&message));
                            }
                            Ok(bollard::container::LogOutput::StdErr { message }) => {
                                stderr.push_str(&String::from_utf8_lossy(&message));
                            }
                            Ok(bollard::container::LogOutput::Console { message }) => {
                                stdout.push_str(&String::from_utf8_lossy(&message));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                return Err(format!("Failed to read exec output: {e}"));
                            }
                        }
                    }
                    Ok(ProcessResult {
                        stdout,
                        stderr,
                        exit_code: 0,
                    })
                }
                Ok(bollard::exec::StartExecResults::Detached) => {
                    Ok(ProcessResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    })
                }
                Err(e) => Err(format!("Failed to execute command: {e}")),
            }
        }) {
            Ok(result) => result,
            Err(e) => ProcessResult {
                stdout: String::new(),
                stderr: e,
                exit_code: -1,
            },
        }
    }

    /// Clean up the container.
    fn cleanup(&self) {
        let state_clone = self.state.clone();
        let _ = Self::block_on(async move {
            let docker = Docker::connect_with_local_defaults()
                .map_err(|e| format!("Docker not available: {e}"))?;

            let cid = {
                let state = state_clone.lock();
                state.container_id.clone()
            };
            if let Some(ref cid) = cid {
                let options = RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                };
                let _ = docker.remove_container(cid, Some(options)).await;
            }
            let mut state = state_clone.lock();
            state.container_id = None;
            Ok(())
        });
    }
}

impl Drop for DockerEnvironment {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Environment for DockerEnvironment {
    fn env_type(&self) -> &str {
        "docker"
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn execute(&self, command: &str, _cwd: Option<&str>, _timeout: Option<u64>) -> ProcessResult {
        self.exec_in_container(command)
    }

    fn is_available(&self) -> bool {
        Self::block_on(async {
            let docker = Docker::connect_with_local_defaults()
                .map_err(|e| format!("{e}"))?;
            docker.ping()
                .await
                .map_err(|e| format!("{e}"))?;
            Ok(())
        }).is_ok()
    }
}

/// List containers managed by Hermez.
pub fn list_hermez_containers() -> Result<Vec<ContainerSummary>, String> {
    DockerEnvironment::block_on(async {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| format!("Docker not available: {e}"))?;

        let label = format!("hermez-agent-{}", std::process::id());
        let mut filters = std::collections::HashMap::new();
        filters.insert("label".to_string(), vec![label]);

        let options = ListContainersOptions {
            all: true,
            limit: None,
            size: false,
            filters,
        };

        docker.list_containers(Some(options))
            .await
            .map_err(|e| format!("Failed to list containers: {e}"))
    })
}

/// Create a Docker environment from config.
pub fn create_docker_env(config: DockerConfig, cwd: Option<&str>) -> DockerEnvironment {
    match cwd {
        Some(dir) => DockerEnvironment::new(config).with_cwd(dir),
        None => DockerEnvironment::new(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_config_default() {
        let config = DockerConfig::default();
        assert_eq!(config.image, DEFAULT_IMAGE);
        assert!(config.bind_mounts.is_empty());
        assert!(config.cpu_limit.is_none());
    }

    #[test]
    fn test_docker_config_custom() {
        let config = DockerConfig {
            image: "python:3.11".to_string(),
            bind_mounts: vec![("/tmp".to_string(), "/workspace".to_string())],
            cpu_limit: Some(0.5),
            memory_limit: Some(512 * 1024 * 1024),
            workdir: Some("/workspace".to_string()),
        };
        assert_eq!(config.image, "python:3.11");
        assert_eq!(config.bind_mounts.len(), 1);
        assert_eq!(config.cpu_limit, Some(0.5));
    }

    #[test]
    fn test_docker_env_type() {
        let config = DockerConfig::default();
        let env = DockerEnvironment::new(config);
        assert_eq!(env.env_type(), "docker");
    }

    #[test]
    fn test_docker_env_cwd() {
        let config = DockerConfig::default();
        let env = DockerEnvironment::new(config).with_cwd("/workspace");
        assert!(env.cwd().to_string_lossy().contains("workspace"));
    }

    #[test]
    fn test_docker_env_timeout() {
        let config = DockerConfig::default();
        let env = DockerEnvironment::new(config).with_timeout(120);
        assert_eq!(env.timeout, 120);
    }

    #[test]
    fn test_create_docker_env() {
        let config = DockerConfig::default();
        let env = create_docker_env(config, Some("/tmp"));
        assert_eq!(env.env_type(), "docker");
    }

    #[test]
    fn test_docker_execute_without_daemon() {
        // Without Docker daemon, execute should fail gracefully
        let config = DockerConfig::default();
        let env = DockerEnvironment::new(config);
        let result = env.execute("echo hello", None, None);
        // Will fail because Docker daemon is likely not available in test env
        assert!(result.exit_code != 0 || !result.stderr.is_empty());
    }

    #[test]
    fn test_docker_is_available_check() {
        let config = DockerConfig::default();
        let env = DockerEnvironment::new(config);
        // May or may not be available depending on test environment
        let available = env.is_available();
        // Just verify it doesn't panic
        if available {
            // Docker is running
        }
    }

    #[test]
    fn test_send_sync() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<DockerEnvironment>();
    }
}
