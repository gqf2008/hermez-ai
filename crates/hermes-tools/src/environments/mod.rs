//! Terminal environment backends.
//!
//! Mirrors the Python `tools/environments/` directory.
//! Implemented: LocalEnvironment, SshEnvironment, DockerEnvironment,
//! DaytonaEnvironment (skeleton), ModalEnvironment (skeleton),
//! SingularityEnvironment.

pub mod daytona;
#[cfg(feature = "docker")]
pub mod docker_env;
pub mod file_sync;
pub mod modal;
pub mod singularity;
pub mod ssh;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Result of executing a command.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Terminal environment trait.
///
/// Each backend (local, Docker, SSH, Modal, etc.) implements this trait.
/// The trait provides a consistent interface for command execution
/// regardless of where the command actually runs.
pub trait Environment: Send + Sync {
    /// Get the environment type name.
    fn env_type(&self) -> &str;

    /// Get the working directory.
    fn cwd(&self) -> &Path;

    /// Execute a command in this environment.
    fn execute(&self, command: &str, cwd: Option<&str>, timeout: Option<u64>) -> ProcessResult;

    /// Check if the environment is available.
    fn is_available(&self) -> bool {
        true
    }
}

/// Environment factory — creates environments from configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnvConfig {
    /// Environment type: "local", "docker", "ssh", "singularity".
    pub env_type: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Docker image (for docker type).
    pub image: Option<String>,
    /// SSH host (for ssh type).
    pub ssh_host: Option<String>,
    /// SSH user (for ssh type).
    pub ssh_user: Option<String>,
    /// SSH port (for ssh type).
    pub ssh_port: Option<u16>,
}

/// Create an environment from configuration.
pub fn create_environment(config: &EnvConfig) -> Arc<dyn Environment> {
    match config.env_type.as_str() {
        "local" => {
            let env = match &config.cwd {
                Some(dir) => LocalEnvironment::new().with_cwd(dir),
                None => LocalEnvironment::new(),
            };
            Arc::new(env)
        }
        "ssh" => {
            let ssh_config = ssh::SshConfig {
                host: config.ssh_host.clone().unwrap_or_default(),
                port: config.ssh_port.unwrap_or(22),
                user: config.ssh_user.clone().unwrap_or_default(),
                auth: None,
            };
            let env = match &config.cwd {
                Some(dir) => ssh::SshEnvironment::new(ssh_config).with_cwd(dir),
                None => ssh::SshEnvironment::new(ssh_config),
            };
            Arc::new(env)
        }
        #[cfg(feature = "docker")]
        "docker" => {
            let docker_config = docker_env::DockerConfig {
                image: config.image.clone().unwrap_or_else(|| "ubuntu:22.04".into()),
                workdir: config.cwd.clone(),
                ..Default::default()
            };
            let env = docker_env::DockerEnvironment::new(docker_config);
            Arc::new(env)
        }
        #[cfg(not(feature = "docker"))]
        "docker" => {
            panic!("Docker support not compiled in. Enable the `docker` feature.")
        }
        "singularity" => {
            let singularity_config = singularity::SingularityConfig {
                image: config.image.clone().unwrap_or_else(|| "docker://ubuntu:22.04".into()),
                cwd: config.cwd.clone().unwrap_or_else(|| "~".into()),
                ..Default::default()
            };
            let env = singularity::SingularityEnvironment::new(singularity_config);
            Arc::new(env)
        }
        _other => {
            Arc::new(LocalEnvironment::new())
        }
    }
}

/// Local environment — runs commands on the host machine.
pub struct LocalEnvironment {
    cwd: PathBuf,
    timeout: u64,
}

impl Default for LocalEnvironment {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalEnvironment {
    /// Create a new local environment.
    pub fn new() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            timeout: 60,
        }
    }

    /// Set the working directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = cwd.as_ref().to_path_buf();
        self
    }

    /// Set the default timeout.
    pub fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Environment for LocalEnvironment {
    fn env_type(&self) -> &str {
        "local"
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn execute(&self, command: &str, cwd: Option<&str>, _timeout: Option<u64>) -> ProcessResult {
        let default_cwd = self.cwd.to_string_lossy().to_string();
        let effective_cwd = cwd.unwrap_or(&default_cwd);

        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", command]);
            c
        };

        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("bash");
            c.args(["-lc", command]);
            c
        };

        cmd.current_dir(effective_cwd);
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
                stderr: format!("Failed to execute command: {e}"),
                exit_code: -1,
            },
        }
    }
}

/// Create a local environment.
pub fn create_local_env(cwd: Option<&str>) -> LocalEnvironment {
    match cwd {
        Some(dir) => LocalEnvironment::new().with_cwd(dir),
        None => LocalEnvironment::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_env_type() {
        let env = LocalEnvironment::new();
        assert_eq!(env.env_type(), "local");
    }

    #[test]
    fn test_local_cwd() {
        let env = LocalEnvironment::new();
        assert!(!env.cwd().to_string_lossy().is_empty());
    }

    #[test]
    fn test_local_execute_command() {
        let env = LocalEnvironment::new();
        #[cfg(windows)]
        let result = env.execute("echo hello", None, None);
        #[cfg(unix)]
        let result = env.execute("echo hello", None, None);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn test_local_execute_with_cwd() {
        let env = LocalEnvironment::new();
        #[cfg(windows)]
        let result = env.execute("cd", None, None);
        #[cfg(unix)]
        let result = env.execute("pwd", None, None);
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn test_local_execute_failing_command() {
        let env = LocalEnvironment::new();
        let result = env.execute("nonexistent_command_xyz", None, None);
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn test_create_local_env() {
        let env = create_local_env(None);
        assert_eq!(env.env_type(), "local");
    }

    #[test]
    fn test_create_local_env_with_cwd() {
        let env = create_local_env(Some("/tmp"));
        assert!(env.cwd().to_string_lossy().contains("tmp"));
    }

    #[test]
    fn test_local_is_available() {
        let env = LocalEnvironment::new();
        assert!(env.is_available());
    }

    #[test]
    fn test_builder_pattern() {
        let env = LocalEnvironment::new()
            .with_cwd(".")
            .with_timeout(30);
        assert_eq!(env.env_type(), "local");
        assert_eq!(env.timeout, 30);
    }

    #[test]
    fn test_send_sync() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<LocalEnvironment>();
        assert_send::<Box<dyn Environment>>();
        assert_send::<Arc<dyn Environment>>();
    }
}
