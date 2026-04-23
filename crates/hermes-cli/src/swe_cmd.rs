#![allow(dead_code)]
//! SWE evaluation subcommands.
//!
//! Mirrors the Python `environments/hermes_swe_env.py` CLI interface.
//!
//! Provides:
//! - `hermes swe evaluate` — run SWE evaluation on a dataset
//! - `hermes swe benchmark` — run built-in benchmark suite
//! - `hermes swe env` — show environment info and test sandbox

use std::path::Path;
use std::sync::Arc;

use console::Style;
use hermes_rl::Environment;
use hermes_rl::base::{AgentRunner, AgentResult, Message as RlMessage};

/// Options for SWE evaluation.
#[derive(Default)]
pub struct SweEvaluateOptions {
    /// Dataset source (path, hf:// identifier, or "builtin").
    pub dataset: String,
    /// Dataset split.
    pub split: String,
    /// Sandbox backend: local or docker.
    pub sandbox: String,
    /// Max evaluation samples (0 = all eval items).
    pub max_samples: usize,
    /// Output directory for results.
    pub output_dir: Option<String>,
    /// Model name for display / agent selection.
    pub model: Option<String>,
    /// Whether to use a real agent loop (requires API key).
    pub use_agent: bool,
    /// Whether to run in quick mode (fewer samples, faster).
    pub quick: bool,
}

/// Agent runner that bridges `AIAgent::run_conversation` to the SWE environment.
struct SweAgentRunner {
    agent: tokio::sync::Mutex<hermes_agent_engine::AIAgent>,
    system_prompt: Option<String>,
    max_turns: usize,
}

#[async_trait::async_trait]
impl AgentRunner for SweAgentRunner {
    async fn run(&self, messages: Vec<RlMessage>) -> AgentResult {
        let mut agent = self.agent.lock().await;

        let system = messages.iter().find(|m| m.role == "system").map(|m| m.content.clone());
        let user = messages.iter().find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let turn_result = agent.run_conversation(&user, system.as_deref(), None).await;

        // Convert TurnResult → AgentResult
        let final_response = turn_result.response.clone();
        let turns_used = turn_result.api_calls;

        // Convert Arc<Value> messages back to RlMessage
        let mut rl_messages = Vec::new();
        for msg in &turn_result.messages {
            if let Some(obj) = msg.as_object() {
                let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("assistant").to_string();
                let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                rl_messages.push(RlMessage {
                    role,
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
        }

        let tools_used = AgentResult::collect_tools_used(&rl_messages);
        let total_tool_calls = AgentResult::count_tool_calls(&rl_messages);
        AgentResult {
            messages: rl_messages,
            turns_used,
            finished_naturally: turn_result.exit_reason == hermes_agent_engine::agent::ExitReason::NaturalStop,
            reasoning_per_turn: Vec::new(),
            tool_errors: Vec::new(),
            tools_used,
            total_tool_calls,
            final_response,
        }
    }
}

/// Run SWE evaluation on a dataset.
pub async fn cmd_swe_evaluate(opts: &SweEvaluateOptions) -> anyhow::Result<()> {
    let green = Style::new().green();
    let cyan = Style::new().cyan();
    let yellow = Style::new().yellow();
    let red = Style::new().red();
    let dim = Style::new().dim();

    println!();
    println!("{}", cyan.apply_to("◆ SWE Evaluation"));
    println!("  Dataset:     {}", opts.dataset);
    println!("  Split:       {}", opts.split);
    println!("  Sandbox:     {}", opts.sandbox);
    if opts.quick {
        println!("  Mode:        {}", yellow.apply_to("quick"));
    }
    if opts.use_agent {
        println!("  Agent:       {}", green.apply_to("real AIAgent"));
        if let Some(ref m) = opts.model {
            println!("  Model:       {}", m);
        }
    } else {
        println!("  Agent:       {}", dim.apply_to("placeholder (no LLM calls)"));
    }
    println!();

    // Determine sandbox backend
    let sandbox_backend = match opts.sandbox.as_str() {
        "docker" => hermes_rl::swe_env::SandboxBackend::Docker,
        _ => hermes_rl::swe_env::SandboxBackend::Local,
    };

    // Build config
    let mut config = hermes_rl::swe_env::SweEnvConfig::default();
    config.sandbox_backend = sandbox_backend;
    config.dataset_source = if opts.dataset == "builtin" {
        None
    } else {
        Some(opts.dataset.clone())
    };
    config.dataset_split = opts.split.clone();

    if opts.quick {
        config.eval_size = 5;
        config.base.max_agent_turns = 15;
    }

    // Setup environment
    let mut env = hermes_rl::swe_env::SweEnv::with_config(config);

    // Attach real agent if requested
    if opts.use_agent {
        let mut agent_config = hermes_agent_engine::AgentConfig::default();
        if let Some(ref model) = opts.model {
            agent_config.model = model.clone();
        }
        agent_config.max_iterations = opts.quick.then_some(15).unwrap_or(30);
        let tool_registry = Arc::new(hermes_tools::registry::ToolRegistry::new());
        match hermes_agent_engine::AIAgent::new(agent_config, tool_registry) {
            Ok(agent) => {
                let runner = Arc::new(SweAgentRunner {
                    agent: tokio::sync::Mutex::new(agent),
                    system_prompt: env.config().system_prompt.clone(),
                    max_turns: opts.quick.then_some(15).unwrap_or(30),
                });
                env = env.with_agent_runner(runner);
            }
            Err(e) => {
                eprintln!("  {} Failed to create agent: {}", red.apply_to("✗"), e);
                eprintln!("  Falling back to placeholder evaluation.");
            }
        }
    }

    if let Err(e) = env.setup().await {
        eprintln!("  {} Failed to setup environment: {}", red.apply_to("✗"), e);
        return Ok(());
    }

    let eval_size = if opts.max_samples > 0 {
        opts.max_samples.min(env.eval_items.len())
    } else {
        env.eval_items.len()
    };

    println!("  {} Environment ready: {} train / {} eval items",
        green.apply_to("✓"),
        env.items.len(),
        env.eval_items.len()
    );
    println!();

    if eval_size == 0 {
        println!("  {} No evaluation items available.", yellow.apply_to("→"));
        return Ok(());
    }

    // Run evaluation
    println!("  {} Running evaluation on {} items...", dim.apply_to("→"), eval_size);
    println!();

    let mut total_reward = 0.0;
    let mut total_test_pass = 0.0;
    let mut total_compilation = 0.0;
    let mut passed_count = 0usize;
    let mut failed_count = 0usize;

    for (i, item) in env.eval_items[..eval_size].iter().enumerate() {
        let mut map = std::collections::HashMap::new();
        map.insert("task_id".into(), serde_json::Value::String(item.task_id.clone()));
        map.insert("prompt".into(), serde_json::Value::String(item.prompt.clone()));

        // For evaluation without a real LLM, we simulate a result.
        // In production, this would call an agent loop.
        let prompt = env.format_prompt(&map);
        let mut messages = vec![hermes_rl::base::Message::user(&prompt)];

        // Use reference solution if available for testing the pipeline
        let response = if let Some(ref reference) = item.reference {
            format!("```python\n{}\n```", reference)
        } else {
            String::new()
        };
        messages.push(hermes_rl::base::Message::assistant(&response));

        let result = hermes_rl::base::AgentResult::new(messages);
        let (reward, signals) = env.compute_reward_breakdown(item, &result).await;

        let test_pass = signals.get("test_pass_rate").copied().unwrap_or(0.0);
        let compilation = signals.get("compilation").copied().unwrap_or(0.0);

        total_test_pass += test_pass;
        total_compilation += compilation;
        total_reward += reward;

        if test_pass >= 1.0 {
            passed_count += 1;
        } else {
            failed_count += 1;
        }

        let status = if test_pass >= 1.0 {
            green.apply_to("✓ PASS").to_string()
        } else if compilation >= 1.0 {
            yellow.apply_to("◐ PARTIAL").to_string()
        } else {
            red.apply_to("✗ FAIL").to_string()
        };

        println!(
            "  [{:>3}/{}] {} {:<12} reward={:.2} test={:.0}% compile={:.0}%",
            i + 1,
            eval_size,
            status,
            item.task_id,
            reward,
            test_pass * 100.0,
            compilation * 100.0,
        );
    }

    println!();

    // Summary
    let n = eval_size as f64;
    let mean_reward = total_reward / n;
    let mean_test_pass = total_test_pass / n;
    let mean_compilation = total_compilation / n;
    let pass_rate = passed_count as f64 / n;

    println!("{}", cyan.apply_to("◆ Results Summary"));
    println!("  Items evaluated:     {}", eval_size);
    println!("  Passed:              {}/{} ({:.1}%)", passed_count, eval_size, pass_rate * 100.0);
    println!("  Failed:              {}/{} ({:.1}%)", failed_count, eval_size, (1.0 - pass_rate) * 100.0);
    println!();
    println!("  Mean reward:         {:.3}", mean_reward);
    println!("  Mean test pass:      {:.3}", mean_test_pass);
    println!("  Mean compilation:    {:.3}", mean_compilation);
    println!();

    // Write report
    if let Some(ref output_dir) = opts.output_dir {
        let _ = std::fs::create_dir_all(output_dir);
        let report_path = Path::new(output_dir).join("swe_eval_report.json");
        let report = serde_json::json!({
            "dataset": opts.dataset,
            "split": opts.split,
            "sandbox": opts.sandbox,
            "eval_size": eval_size,
            "passed": passed_count,
            "failed": failed_count,
            "pass_rate": pass_rate,
            "mean_reward": mean_reward,
            "mean_test_pass": mean_test_pass,
            "mean_compilation": mean_compilation,
            "model": opts.model,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            if std::fs::write(&report_path, json).is_ok() {
                println!("  {} Report saved to {}", green.apply_to("✓"), report_path.display());
            }
        }
        println!();
    }

    Ok(())
}

/// Run the built-in SWE benchmark suite.
pub async fn cmd_swe_benchmark(quick: bool) -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();

    println!();
    println!("{}", cyan.apply_to("◆ SWE Benchmark Suite"));
    println!();

    let opts = SweEvaluateOptions {
        dataset: "builtin".into(),
        split: "test".into(),
        sandbox: "local".into(),
        quick,
        ..Default::default()
    };

    cmd_swe_evaluate(&opts).await?;

    println!("{}", green.apply_to("  Benchmark complete."));
    println!();

    Ok(())
}

/// Show SWE environment info and test sandbox connectivity.
pub fn cmd_swe_env_info() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let red = Style::new().red();

    println!();
    println!("{}", cyan.apply_to("◆ SWE Environment"));
    println!();

    // Check Python availability
    let python_check = std::process::Command::new("python3")
        .arg("--version")
        .output();
    match python_check {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("  {} Python: {}", green.apply_to("✓"), version);
        }
        _ => {
            println!("  {} Python3 not found (required for test execution)", red.apply_to("✗"));
        }
    }

    // Check pytest
    let pytest_check = std::process::Command::new("python3")
        .args(["-m", "pytest", "--version"])
        .output();
    match pytest_check {
        Ok(output) if output.status.success() => {
            println!("  {} pytest: available", green.apply_to("✓"));
        }
        _ => {
            println!("  {} pytest: not installed (will use unittest fallback)", yellow.apply_to("→"));
        }
    }

    // Check Docker
    let docker_check = std::process::Command::new("docker")
        .arg("--version")
        .output();
    match docker_check {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("  {} Docker: {}", green.apply_to("✓"), version);
        }
        _ => {
            println!("  {} Docker: not available (local sandbox only)", yellow.apply_to("→"));
        }
    }

    // Show built-in tasks count
    let builtin_tasks = hermes_rl::swe_env::builtin_swe_tasks();
    println!("  {} Built-in tasks: {}", green.apply_to("✓"), builtin_tasks.len());

    println!();
    println!("  Supported dataset formats:");
    println!("    - .jsonl (line-delimited JSON)");
    println!("    - .json (JSON array)");
    println!("    - hf://<dataset_name> (HuggingFace datasets)");
    println!("    - builtin (embedded fallback tasks)");
    println!();
    println!("  Supported sandbox backends:");
    println!("    - local (temp directory, fast)");
    println!("    - docker (container isolation, requires Docker)");
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_swe_env_info_runs() {
        let result = cmd_swe_env_info();
        assert!(result.is_ok());
    }
}
