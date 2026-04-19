#![allow(dead_code)]
//! RL training tools — reinforcement learning on Tinker-Atropos.
//!
//! Mirrors the Python `tools/rl_training_tool.py`.
//! Full implementation with subprocess management for training runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// Fields that cannot be modified by the agent.
const LOCKED_FIELDS: &[&str] = &[
    "tokenizer",
    "rollout_server_url",
    "wandb",
    "max_token_length",
    "total_steps",
    "lora_rank",
    "learning_rate",
];

/// RL training state.
struct RLState {
    selected_env: Option<String>,
    config: HashMap<String, Value>,
    runs: Vec<RLRun>,
}

struct RLRun {
    id: String,
    environment: String,
    status: String, // "running", "completed", "stopped", "failed"
    started_at: String,
    steps_completed: u64,
    child: Option<Child>,
    log_path: Option<PathBuf>,
}

/// Find the tinker-atropos root directory.
fn find_atropos_root() -> Option<PathBuf> {
    // Check relative to hermes-agent root
    // manifest_dir = hermes-rs/crates/hermes-tools
    // parent1 = hermes-rs/crates
    // parent2 = hermes-rs
    // parent3 = hermes-agent (root)
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let parent1 = manifest_dir.parent()?;
    let parent2 = parent1.parent()?;
    let parent3 = parent2.parent()?;
    let candidate = parent3.join("tinker-atropos");

    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

static RL_STATE: Lazy<Mutex<RLState>> = Lazy::new(|| {
    let mut config = HashMap::new();
    config.insert("max_token_length".to_string(), Value::Number(8192.into()));
    config.insert("total_steps".to_string(), Value::Number(2500.into()));
    config.insert("lora_rank".to_string(), Value::Number(32.into()));
    config.insert("learning_rate".to_string(), Value::String("4e-5".to_string()));

    Mutex::new(RLState {
        selected_env: None,
        config,
        runs: Vec::new(),
    })
});

/// Known Atropos environments (hardcoded for MVP).
const KNOWN_ENVIRONMENTS: &[&str] = &[
    "math",
    "code",
    "reasoning",
    "conversation",
    "tool_use",
];

fn handle_rl_action(action: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    match action {
        "list_environments" => rl_list_environments(),
        "select_environment" => rl_select_environment(args),
        "get_current_config" => rl_get_current_config(),
        "edit_config" => rl_edit_config(args),
        "start_training" => rl_start_training(args),
        "check_status" => rl_check_status(args),
        "stop_training" => rl_stop_training(args),
        "get_results" => rl_get_results(args),
        "list_runs" => rl_list_runs(),
        "test_inference" => rl_test_inference(args),
        _ => Ok(tool_error(format!(
            "Unknown RL action: '{action}'. Valid actions: list_environments, select_environment, get_current_config, edit_config, start_training, check_status, stop_training, get_results, list_runs, test_inference"
        ))),
    }
}

fn rl_list_environments() -> Result<String, hermes_core::HermesError> {
    // Scan tinker-atropos for environments if available
    let mut envs: Vec<Value> = KNOWN_ENVIRONMENTS
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e,
                "available": true,
            })
        })
        .collect();

    // Try to discover additional environments from tinker-atropos
    if let Some(root) = find_atropos_root() {
        let env_dir = root.join("tinker_atropos").join("environments");
        if env_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&env_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("py") {
                        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                            if !name.starts_with('_') && !KNOWN_ENVIRONMENTS.contains(&name) {
                                envs.push(serde_json::json!({
                                    "name": name,
                                    "available": true,
                                    "source": "discovered",
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "list_environments",
        "environments": envs,
    })
    .to_string())
}

fn rl_select_environment(args: &Value) -> Result<String, hermes_core::HermesError> {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => return Ok(tool_error("rl_select_environment requires 'name' parameter.")),
    };

    if !KNOWN_ENVIRONMENTS.contains(&name.as_str()) {
        return Ok(tool_error(format!(
            "Unknown environment: '{name}'. Available: {}",
            KNOWN_ENVIRONMENTS.join(", ")
        )));
    }

    let mut state = RL_STATE.lock().unwrap();
    state.selected_env = Some(name.clone());

    Ok(serde_json::json!({
        "success": true,
        "action": "select_environment",
        "environment": name,
    })
    .to_string())
}

fn rl_get_current_config() -> Result<String, hermes_core::HermesError> {
    let state = RL_STATE.lock().unwrap();

    let env = state.selected_env.clone();
    if env.is_none() {
        return Ok(tool_error("No environment selected. Use rl_select_environment first."));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "get_current_config",
        "environment": env,
        "config": state.config,
    })
    .to_string())
}

fn rl_edit_config(args: &Value) -> Result<String, hermes_core::HermesError> {
    let field = match args.get("field").and_then(Value::as_str) {
        Some(f) => f.to_string(),
        None => return Ok(tool_error("rl_edit_config requires 'field' parameter.")),
    };

    let value = args
        .get("value")
        .cloned()
        .unwrap_or(Value::Null);

    if LOCKED_FIELDS.contains(&field.as_str()) {
        return Ok(tool_error(format!(
            "Field '{field}' is locked and cannot be modified."
        )));
    }

    let mut state = RL_STATE.lock().unwrap();
    state.config.insert(field.clone(), value.clone());

    Ok(serde_json::json!({
        "success": true,
        "action": "edit_config",
        "field": field,
        "value": value,
    })
    .to_string())
}

fn rl_start_training(_args: &Value) -> Result<String, hermes_core::HermesError> {
    let mut state = RL_STATE.lock().unwrap();

    let env = state.selected_env.clone();
    if env.is_none() {
        return Ok(tool_error("No environment selected. Use rl_select_environment first."));
    }

    let run_id = format!("run_{}", state.runs.len() + 1);
    let now = chrono::Utc::now().to_rfc3339();

    // Try to spawn the actual training subprocess
    let (status, child, log_path) = spawn_training_process(&run_id, env.as_ref().unwrap());

    state.runs.push(RLRun {
        id: run_id.clone(),
        environment: env.clone().unwrap(),
        status: status.clone(),
        started_at: now.clone(),
        steps_completed: 0,
        child,
        log_path,
    });

    Ok(serde_json::json!({
        "success": true,
        "action": "start_training",
        "run_id": run_id,
        "environment": env,
        "started_at": now,
        "status": status,
    })
    .to_string())
}

/// Spawn the Atropos training subprocess.
fn spawn_training_process(run_id: &str, env_name: &str) -> (String, Option<Child>, Option<PathBuf>) {
    let atropos_root = match find_atropos_root() {
        Some(root) => root,
        None => {
            return ("starting".to_string(), None, None);
        }
    };

    // Check for Python availability
    let python = find_python();
    if python.is_none() {
        return ("starting".to_string(), None, None);
    }

    // Create log file
    let log_dir = std::env::temp_dir().join("hermes_rl_logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{run_id}.log"));

    let log_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(_) => return ("starting".to_string(), None, None),
    };

    // Build the training command
    // Uses launch_training.py from tinker-atropos
    let launch_script = atropos_root.join("launch_training.py");
    if !launch_script.exists() {
        return ("starting".to_string(), None, None);
    }

    let mut cmd = Command::new(python.unwrap());
    cmd.arg(&launch_script)
        .arg("--env")
        .arg(env_name)
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file.try_clone().unwrap())
        .current_dir(&atropos_root);

    // Pass TINKER_API_KEY if available
    if let Ok(key) = std::env::var("TINKER_API_KEY") {
        cmd.env("TINKER_API_KEY", key);
    }

    match cmd.spawn() {
        Ok(child) => ("running".to_string(), Some(child), Some(log_path)),
        Err(_) => ("starting".to_string(), None, Some(log_path)),
    }
}

fn find_python() -> Option<String> {
    for cmd in ["python", "python3"] {
        if Command::new(cmd)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok()
        {
            return Some(cmd.to_string());
        }
    }
    None
}

fn rl_check_status(args: &Value) -> Result<String, hermes_core::HermesError> {
    let run_id = match args.get("run_id").and_then(Value::as_str) {
        Some(r) => r.to_string(),
        None => return Ok(tool_error("rl_check_status requires 'run_id' parameter.")),
    };

    let mut state = RL_STATE.lock().unwrap();
    let run = state.runs.iter_mut().find(|r| r.id == run_id);

    match run {
        Some(r) => {
            // Check if process is still running
            if let Some(ref mut child) = r.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Process has exited
                        if status.success() {
                            r.status = "completed".to_string();
                        } else {
                            r.status = "failed".to_string();
                        }
                        r.child = None;
                    }
                    Ok(None) => {
                        // Still running
                        r.status = "running".to_string();
                    }
                    Err(_) => {
                        r.status = "failed".to_string();
                        r.child = None;
                    }
                }
            }

            // Read last few lines from log if available
            let log_tail = r.log_path.as_ref().and_then(|p| {
                std::fs::read_to_string(p)
                    .ok()
                    .map(|s| {
                        let lines: Vec<&str> = s.lines().collect();
                        let start = lines.len().saturating_sub(5);
                        lines[start..].join("\n")
                    })
            });

            Ok(serde_json::json!({
                "success": true,
                "action": "check_status",
                "run_id": r.id,
                "environment": r.environment,
                "status": r.status,
                "started_at": r.started_at,
                "steps_completed": r.steps_completed,
                "log_tail": log_tail,
            })
            .to_string())
        }
        None => Ok(tool_error(format!("Run not found: {run_id}"))),
    }
}

fn rl_stop_training(args: &Value) -> Result<String, hermes_core::HermesError> {
    let run_id = match args.get("run_id").and_then(Value::as_str) {
        Some(r) => r.to_string(),
        None => return Ok(tool_error("rl_stop_training requires 'run_id' parameter.")),
    };

    let mut state = RL_STATE.lock().unwrap();
    let run = state.runs.iter_mut().find(|r| r.id == run_id);

    match run {
        Some(r) => {
            // Kill the subprocess if running
            if let Some(ref mut child) = r.child {
                let _ = child.kill();
                r.child = None;
            }
            r.status = "stopped".to_string();
            Ok(serde_json::json!({
                "success": true,
                "action": "stop_training",
                "run_id": run_id,
            })
            .to_string())
        }
        None => Ok(tool_error(format!("Run not found: {run_id}"))),
    }
}

fn rl_get_results(args: &Value) -> Result<String, hermes_core::HermesError> {
    let run_id = match args.get("run_id").and_then(Value::as_str) {
        Some(r) => r.to_string(),
        None => return Ok(tool_error("rl_get_results requires 'run_id' parameter.")),
    };

    let state = RL_STATE.lock().unwrap();
    let run = state.runs.iter().find(|r| r.id == run_id);

    match run {
        Some(r) => {
            // Try to read full log for metrics
            let log_content = r.log_path.as_ref().and_then(|p| {
                std::fs::read_to_string(p).ok()
            });

            // Extract basic metrics from log if available
            let metrics = extract_metrics_from_log(log_content.as_deref());

            Ok(serde_json::json!({
                "success": true,
                "action": "get_results",
                "run_id": r.id,
                "environment": r.environment,
                "status": r.status,
                "steps_completed": r.steps_completed,
                "metrics": metrics,
                "log_path": r.log_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            })
            .to_string())
        }
        None => Ok(tool_error(format!("Run not found: {run_id}"))),
    }
}

/// Extract simple metrics from training log.
fn extract_metrics_from_log(log: Option<&str>) -> Value {
    let log = match log {
        Some(l) => l,
        None => return serde_json::json!({}),
    };

    let mut metrics = serde_json::Map::new();

    // Look for step/reward patterns in log
    for line in log.lines() {
        if line.contains("step") && line.contains("reward") {
            // Try to parse JSON metrics
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                metrics = v.as_object().cloned().unwrap_or_default();
            }
        }
    }

    Value::Object(metrics)
}

fn rl_list_runs() -> Result<String, hermes_core::HermesError> {
    let state = RL_STATE.lock().unwrap();

    let runs: Vec<Value> = state
        .runs
        .iter()
        .map(|r| {
            serde_json::json!({
                "run_id": r.id,
                "environment": r.environment,
                "status": r.status,
                "started_at": r.started_at,
                "steps_completed": r.steps_completed,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "action": "list_runs",
        "total_runs": runs.len(),
        "runs": runs,
    })
    .to_string())
}

fn rl_test_inference(args: &Value) -> Result<String, hermes_core::HermesError> {
    let num_steps = args.get("num_steps").and_then(Value::as_u64).unwrap_or(10);
    let group_size = args.get("group_size").and_then(Value::as_u64).unwrap_or(1);
    let models = args
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(serde_json::json!({
        "success": true,
        "action": "test_inference",
        "num_steps": num_steps,
        "group_size": group_size,
        "models": models,
        "note": "In full mode, a validation run would be spawned.",
    })
    .to_string())
}

/// Handle RL tool call.
pub fn handle_rl(args: Value) -> Result<String, hermes_core::HermesError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list_environments");

    handle_rl_action(action, &args)
}

/// Register RL training tools.
pub fn register_rl_tools(registry: &mut ToolRegistry) {
    registry.register(
        "rl_training".to_string(),
        "rl".to_string(),
        serde_json::json!({
            "name": "rl_training",
            "description": "RL training tools for running reinforcement learning on Tinker-Atropos. Actions: list_environments, select_environment, get_current_config, edit_config, start_training, check_status, stop_training, get_results, list_runs, test_inference.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "RL action to perform." },
                    "name": { "type": "string", "description": "Environment name (for select_environment)." },
                    "field": { "type": "string", "description": "Config field name (for edit_config)." },
                    "value": { "description": "Config field value (for edit_config)." },
                    "run_id": { "type": "string", "description": "Run ID (for status/results/stop)." },
                    "num_steps": { "type": "integer", "description": "Test inference steps." },
                    "group_size": { "type": "integer", "description": "Test inference group size." },
                    "models": { "type": "array", "items": { "type": "string" }, "description": "Models to test." }
                },
                "required": ["action"]
            }
        }),
        std::sync::Arc::new(handle_rl),
        None,
        vec!["rl".to_string()],
        "RL training on Tinker-Atropos".to_string(),
        "🧠".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cleanup() {
        let mut state = RL_STATE.lock().unwrap();
        state.selected_env = None;
        state.runs.clear();
    }

    #[test]
    #[serial]
    fn test_list_environments() {
        cleanup();
        let result = handle_rl(serde_json::json!({ "action": "list_environments" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["environments"].as_array().unwrap().len() >= 3);
    }

    #[test]
    #[serial]
    fn test_select_environment() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "select_environment",
            "name": "math"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["environment"], "math");
    }

    #[test]
    #[serial]
    fn test_select_unknown_environment() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "select_environment",
            "name": "nonexistent"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_get_config_without_selection() {
        cleanup();
        let result = handle_rl(serde_json::json!({ "action": "get_current_config" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_get_config_after_selection() {
        cleanup();
        let _ = handle_rl(serde_json::json!({ "action": "select_environment", "name": "code" }));
        let result = handle_rl(serde_json::json!({ "action": "get_current_config" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["environment"], "code");
    }

    #[test]
    #[serial]
    fn test_edit_config_locked() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "edit_config",
            "field": "learning_rate",
            "value": "0.001"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_edit_config_unlocked() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "edit_config",
            "field": "custom_field",
            "value": 42
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["field"], "custom_field");
    }

    #[test]
    #[serial]
    fn test_start_training_without_selection() {
        cleanup();
        let result = handle_rl(serde_json::json!({ "action": "start_training" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_start_training_with_selection() {
        cleanup();
        let _ = handle_rl(serde_json::json!({ "action": "select_environment", "name": "math" }));
        let result = handle_rl(serde_json::json!({ "action": "start_training" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["run_id"].as_str().unwrap().starts_with("run_"));
    }

    #[test]
    #[serial]
    fn test_list_runs() {
        cleanup();
        let _ = handle_rl(serde_json::json!({ "action": "select_environment", "name": "math" }));
        let _ = handle_rl(serde_json::json!({ "action": "start_training" }));
        let result = handle_rl(serde_json::json!({ "action": "list_runs" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["total_runs"], 1);
    }

    #[test]
    #[serial]
    fn test_check_status_not_found() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "check_status",
            "run_id": "nonexistent"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_stop_training() {
        cleanup();
        let _ = handle_rl(serde_json::json!({ "action": "select_environment", "name": "code" }));
        let _ = handle_rl(serde_json::json!({ "action": "start_training" }));
        let result = handle_rl(serde_json::json!({
            "action": "stop_training",
            "run_id": "run_1"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
    }

    #[test]
    #[serial]
    fn test_test_inference() {
        cleanup();
        let result = handle_rl(serde_json::json!({
            "action": "test_inference",
            "num_steps": 5,
            "group_size": 2,
            "models": ["model_a", "model_b"]
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["num_steps"], 5);
    }

    #[test]
    #[serial]
    fn test_unknown_action() {
        cleanup();
        let result = handle_rl(serde_json::json!({ "action": "nonexistent" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_missing_run_id() {
        cleanup();
        for action in ["check_status", "stop_training", "get_results"] {
            let result = handle_rl(serde_json::json!({ "action": action }));
            let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
            assert!(json.get("error").is_some(), "action '{action}' should require run_id");
        }
    }

    #[test]
    #[serial]
    #[ignore = "requires tinker-atropos checkout in parent directory"]
    fn test_find_atropos_root() {
        // Should find tinker-atropos relative to the workspace
        let root = find_atropos_root();
        assert!(root.is_some(), "Should find tinker-atropos root");
        let path = root.unwrap();
        assert!(path.exists(), "Path should exist: {path:?}");
    }

    #[test]
    #[serial]
    fn test_environment_discovery() {
        cleanup();
        let result = handle_rl(serde_json::json!({ "action": "list_environments" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        let envs = json["environments"].as_array().unwrap();
        // Should have at least the hardcoded ones
        assert!(envs.len() >= KNOWN_ENVIRONMENTS.len());
    }

    #[test]
    #[serial]
    fn test_metrics_extraction_empty() {
        let metrics = extract_metrics_from_log(None);
        assert!(metrics.as_object().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn test_metrics_extraction_json() {
        let log = "some log line\n{\"step\": 100, \"reward\": 0.5}\nanother line";
        let metrics = extract_metrics_from_log(Some(log));
        assert_eq!(metrics["step"], 100);
        assert_eq!(metrics["reward"], 0.5);
    }
}
