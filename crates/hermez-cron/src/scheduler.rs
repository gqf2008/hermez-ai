//! Cron scheduler — tick loop, file locking, job execution.
//!
//! Mirrors the Python `cron/scheduler.py`.

use std::path::Path;
use std::sync::Arc;

use hermez_core::{HermezError, Result};

use crate::jobs::{JobStore, save_job_output};

/// Run the scheduler tick loop.
///
/// This is the main entry point for the cron scheduler.
/// It runs a continuous loop, checking for due jobs and executing them.
///
/// # Arguments
/// * `verbose` — Enable verbose logging
/// * `loop_forever` — If true, run continuously; if false, run once and exit
pub async fn run_scheduler(verbose: bool, loop_forever: bool) -> Result<()> {
    let mut store = JobStore::new()?;

    if verbose {
        tracing::info!(
            "Cron scheduler started ({} jobs loaded)",
            store.list(true).len()
        );
    }

    loop {
        let tick_result = tick(&mut store, verbose).await;

        if verbose && tick_result > 0 {
            tracing::info!("Tick: {tick_result} job(s) executed");
        }

        if !loop_forever {
            break;
        }

        // Sleep for 30 seconds before next tick
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }

    Ok(())
}

/// Execute a single scheduler tick with a fresh JobStore.
///
/// Convenience wrapper for external callers (e.g. the gateway) that
/// don't want to manage a persistent `JobStore`.
///
/// Returns the number of jobs executed.
pub async fn tick_once() -> usize {
    match JobStore::new() {
        Ok(mut store) => tick(&mut store, false).await,
        Err(e) => {
            tracing::warn!("Cron tick_once: failed to open job store: {e}");
            0
        }
    }
}

/// Execute a single scheduler tick.
///
/// 1. Acquire file-based lock (skip if already running)
/// 2. Find due jobs
/// 3. Execute each due job
/// 4. Save output
/// 5. Mark job as run
async fn tick(store: &mut JobStore, _verbose: bool) -> usize {
    // Acquire lock
    let lock_path = get_lock_path();
    if !acquire_lock(&lock_path) {
        tracing::debug!("Scheduler lock is held, skipping tick");
        return 0;
    }

    let (due_jobs, fast_forwarded) = store.get_due_jobs_with_fast_forward();
    if _verbose && !fast_forwarded.is_empty() {
        tracing::info!("{} job(s) fast-forwarded (missed grace window)", fast_forwarded.len());
    }
    // Collect job data to avoid borrow conflicts
    let job_ids: Vec<String> = due_jobs.iter().map(|j| j.id.clone()).collect();
    let count = job_ids.len();

    for job_id in job_ids {
        // Advance next_run BEFORE execution (crash safety)
        let _ = store.advance_next_run(&job_id);

        // Get job reference for execution
        let job = store.get(&job_id).cloned();
        let Some(job) = job else { continue };

        // Execute the job
        let result = run_job(&job).await;

        // Save output
        let (success, output, final_response, error) = match result {
            Ok((ok, out, resp, err)) => (ok, out, resp, err),
            Err(e) => (false, format!("Error: {e}"), String::new(), Some(e.to_string())),
        };

        // Save job output
        if !output.is_empty() {
            let _ = save_job_output(&job_id, &output);
        }

        // Check for [SILENT] marker — case-insensitive on delivery content,
        // not raw output (mirrors Python: SILENT_MARKER in deliver_content.strip().upper())
        let delivery_content = if success {
            final_response.clone()
        } else {
            format!("Cron job '{}' failed:\n{}", job.name, error.as_deref().unwrap_or("unknown error"))
        };
        let mut delivery_error = if !delivery_content.is_empty()
            && delivery_content.trim().to_uppercase().contains("[SILENT]")
        {
            Some("silent".to_string())
        } else {
            None
        };

        // Deliver the result to the configured target (origin, platform, or local)
        if delivery_error.is_none() && !delivery_content.is_empty() {
            let target = crate::delivery::resolve_delivery_target(&job.deliver, job.origin.as_ref());
            if let crate::delivery::DeliveryTarget::Local = target {
                // local — no delivery needed
            } else {
                if let Some(err) = crate::delivery::deliver_result(&target, &job.name, &delivery_content).await {
                    tracing::warn!("Cron delivery failed for job {}: {err}", job.id);
                    delivery_error = Some(err);
                }
            }
        }

        // Mark job as run
        let _ = store.mark_run(&job_id, success, error.as_deref(), delivery_error.as_deref());
    }

    // Release lock
    release_lock(&lock_path);

    count
}

/// Execute a single cron job.
///
/// Returns (success, full_output, final_response, error).
async fn run_job(job: &crate::jobs::CronJob) -> Result<(bool, String, String, Option<String>)> {
    tracing::info!("Running cron job: {} ({})", job.name, job.id);

    // Build the prompt: script output + cron guidance + skills + user prompt
    let prompt = build_job_prompt(job);

    // Build agent config
    let model = job.model.clone().unwrap_or_else(|| "anthropic/claude-opus-4.6".to_string());
    let config = hermez_agent_engine::agent::AgentConfig {
        model,
        provider: job.provider.clone(),
        base_url: job.base_url.clone(),
        max_iterations: 50,
        skip_context_files: true,
        platform: Some("cron".to_string()),
        ..hermez_agent_engine::agent::AgentConfig::default()
    };

    // Build tool registry — disable cronjob, messaging, clarify toolsets for cron
    let mut registry = hermez_tools::registry::ToolRegistry::new();
    hermez_tools::register_all_tools(&mut registry);

    let mut agent = hermez_agent_engine::AIAgent::new(config, Arc::new(registry))
        .map_err(|e| HermezError::new(hermez_core::errors::ErrorCategory::InternalError, e.to_string()))?;

    // Run with timeout
    let turn_result = tokio::time::timeout(
        std::time::Duration::from_secs(600), // 10 min default timeout
        agent.run_conversation(&prompt, None, None),
    )
    .await;

    match turn_result {
        Ok(result) => {
            let final_response = if result.response.is_empty() {
                // Get last assistant message
                result
                    .messages
                    .iter()
                    .rev()
                    .find_map(|msg| {
                        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                            msg.get("content").and_then(|v| v.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default()
            } else {
                result.response.clone()
            };

            // Build full output document for local audit
            let output = format!(
                "# Cron Job: {}\n\n\
                 **Job ID:** {}\n\
                 **Run Time:** {}\n\
                 **Schedule:** {}\n\n\
                 ## Prompt\n\n\
                 {prompt}\n\n\
                 ## Response\n\n\
                 {}\n",
                job.name,
                job.id,
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                job.schedule_display,
                if final_response.is_empty() { "(No response generated)" } else { &final_response },
            );

            let success = result.exit_reason == hermez_agent_engine::agent::ExitReason::Completed;
            let error = if success {
                None
            } else {
                Some(format!("Exit: {}", result.exit_reason))
            };

            Ok((success, output, final_response, error))
        }
        Err(_) => {
            let output = format!(
                "# Cron Job: {} (FAILED)\n\n\
                 **Job ID:** {}\n\
                 **Run Time:** {}\n\
                 **Schedule:** {}\n\n\
                 ## Error\n\n\
                 ```\nJob timed out (600s)\n```\n",
                job.name, job.id,
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                job.schedule_display,
            );
            Ok((false, output, String::new(), Some("Job timed out (600s)".to_string())))
        }
    }
}

/// Build the effective prompt for a cron job, including script output,
/// cron execution guidance, and optional skill loading.
fn build_job_prompt(job: &crate::jobs::CronJob) -> String {
    let mut prompt = job.prompt.clone();

    // Run pre-script if configured
    if let Some(ref script) = job.script {
        match run_job_script(script) {
            Ok(script_output) => {
                if script_output.is_empty() {
                    prompt = format!("[Script ran successfully but produced no output.]\n\n{prompt}");
                } else {
                    prompt = format!(
                        "## Script Output\n\
                         The following data was collected by a pre-run script. \
                         Use it as context for your analysis.\n\n\
                         ```\n{script_output}\n```\n\n\
                         {prompt}"
                    );
                }
            }
            Err(e) => {
                prompt = format!(
                    "## Script Error\n\
                     The data-collection script failed. Report this to the user.\n\n\
                     ```\n{e}\n```\n\n\
                     {prompt}"
                );
            }
        }
    }

    // Always prepend cron execution guidance (mirrors Python cron_hint)
    let cron_hint =
        "[SYSTEM: You are running as a scheduled cron job. \
         DELIVERY: Your final response will be automatically delivered \
         to the user — do NOT use send_message or try to deliver \
         the output yourself. Just produce your report/output as your \
         final response and the system handles the rest. \
         SILENT: If there is genuinely nothing new to report, respond \
         with exactly \"[SILENT]\" (nothing else) to suppress delivery. \
         Never combine [SILENT] with content — either report your \
         findings normally, or say [SILENT] and nothing more.]\n\n";
    prompt = format!("{cron_hint}{prompt}");

    // Load skills if specified
    for skill_name in &job.skills {
        let skill_name = skill_name.trim();
        if skill_name.is_empty() {
            continue;
        }
        // Try to load skill content via skills tool
        match load_skill_content(skill_name) {
            Ok(content) => {
                prompt = format!(
                    "{prompt}\n\n\
                     [SYSTEM: The user has invoked the \"{skill_name}\" skill, \
                     indicating they want you to follow its instructions. \
                     The full skill content is loaded below.]\n\n\
                     {content}"
                );
            }
            Err(e) => {
                tracing::warn!("Cron job '{}': skill '{skill_name}' not found: {e}", job.name);
            }
        }
    }

    prompt
}

/// Load skill content by name. Returns the skill content as a string.
fn load_skill_content(skill_name: &str) -> Result<String> {
    // Try to read skill file from ~/.hermez/skills/
    let home = hermez_core::get_hermez_home();
    let skill_path = home.join("skills").join(format!("{skill_name}.md"));
    if skill_path.exists() {
        return std::fs::read_to_string(&skill_path).map_err(|e| {
            HermezError::new(hermez_core::errors::ErrorCategory::InternalError, format!("Failed to read skill: {e}"))
        });
    }

    // Try bundled skills directory
    let bundled = home.join("hermez-agent").join("skills").join(skill_name);
    if bundled.exists() {
        let mut content = String::new();
        for entry in std::fs::read_dir(&bundled).map_err(|e| {
            HermezError::new(hermez_core::errors::ErrorCategory::InternalError, format!("Failed to read skill dir: {e}"))
        })?.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "md") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    content.push_str(&text);
                    content.push_str("\n\n");
                }
            }
        }
        if !content.is_empty() {
            return Ok(content);
        }
    }

    Err(HermezError::new(
        hermez_core::errors::ErrorCategory::InternalError,
        format!("Skill not found: {skill_name}"),
    ))
}

/// Run a pre-job script and return its stdout.
fn run_job_script(script_path: &str) -> Result<String> {
    let home = hermez_core::get_hermez_home();
    let scripts_dir = home.join("scripts");

    // Resolve relative paths against HERMEZ_HOME/scripts/
    let raw = std::path::Path::new(&script_path);
    let full_path = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        scripts_dir.join(script_path)
    };

    // Validate script is within scripts directory (path traversal guard)
    let canonical = full_path.canonicalize().map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Script not found: {e}"),
        )
    })?;

    if !canonical.starts_with(&scripts_dir) {
        return Err(HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            "Script path traversal detected".to_string(),
        ));
    }

    // Determine timeout: env var → default 120s
    let _timeout_secs = std::env::var("HERMEZ_CRON_SCRIPT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);

    // Execute the script using Python interpreter (mirrors Python: sys.executable)
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&canonical)
        .current_dir(canonical.parent().unwrap_or(&scripts_dir));

    let output = cmd.output().map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to execute script: {e}"),
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Redact secrets from both stdout and stderr
    let sanitized_stdout = hermez_core::redact_sensitive_text(&stdout);
    let sanitized_stderr = hermez_core::redact_sensitive_text(&stderr);

    let result = if output.status.success() {
        sanitized_stdout
    } else {
        let mut parts = vec![format!("Script exited with code {}", output.status.code().unwrap_or(-1))];
        if !sanitized_stderr.is_empty() {
            parts.push(format!("stderr:\n{sanitized_stderr}"));
        }
        if !sanitized_stdout.is_empty() {
            parts.push(format!("stdout:\n{sanitized_stdout}"));
        }
        parts.join("\n")
    };

    Ok(result)
}

/// Get the lock file path.
fn get_lock_path() -> std::path::PathBuf {
    let home = hermez_core::get_hermez_home();
    home.join("cron").join(".tick.lock")
}

/// Acquire a file-based lock.
///
/// Returns false if the lock is already held.
fn acquire_lock(path: &Path) -> bool {
    // Try to create the lock file exclusively
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            // Write PID to lock file
            let pid = std::process::id();
            let _ = std::io::Write::write_all(
                &mut std::io::BufWriter::new(file),
                format!("{pid}").as_bytes(),
            );
            true
        }
        Err(_) => false,
    }
}

/// Release a file-based lock.
fn release_lock(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Trigger a job to run on the next tick (one-shot execution).
///
/// Sets next_run_at to now so the scheduler picks it up on the next tick,
/// rather than executing inline (mirrors Python trigger_job behavior).
pub async fn trigger_job(store: &mut JobStore, job_id: &str) -> Result<()> {
    // Check job exists
    store.get(job_id).ok_or_else(|| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Job not found: {job_id}"),
        )
    })?;

    // Set next_run_at to now so it runs on next tick
    let now = chrono::Utc::now().to_rfc3339();
    store.update(job_id, crate::jobs::JobUpdates {
        prompt: None,
        name: None,
        model: None,
        schedule: None,
        deliver: None,
        enabled: None,
        skills: None,
        next_run_override: Some(now),
    })
}

/// List all saved outputs for a job.
pub fn list_job_outputs(job_id: &str) -> Result<Vec<std::path::PathBuf>> {
    let dir = crate::jobs::get_output_dir(job_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to read output directory: {e}"),
            )
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .collect();

    files.sort();
    Ok(files)
}

/// Read a job's output file.
pub fn read_job_output(job_id: &str, filename: &str) -> Result<String> {
    let dir = crate::jobs::get_output_dir(job_id);
    let path = dir.join(filename);

    std::fs::read_to_string(&path).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to read output file: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_sensitive_text() {
        let input = "Here is my key: sk-1234567890abcdef and more text";
        let output = hermez_core::redact_sensitive_text(input);
        // Core redaction uses "***" for short tokens or "prefix...suffix" for long ones
        assert!(!output.contains("sk-1234567890abcdef"));
        assert!(output.contains("sk-") || output.contains("***"));
    }

    #[test]
    fn test_redact_bearer_token() {
        let input = "Authorization: Bearer secret_token_123456789 end";
        let output = hermez_core::redact_sensitive_text(input);
        assert!(!output.contains("secret_token_123456789"));
        assert!(output.contains("Authorization:"));
    }

    #[test]
    fn test_acquire_and_release_lock() {
        let dir = std::env::temp_dir();
        let lock_path = dir.join("test_cron.lock");
        let _ = std::fs::remove_file(&lock_path);

        // First acquire should succeed
        assert!(acquire_lock(&lock_path));
        assert!(lock_path.exists());

        // Second acquire should fail
        assert!(!acquire_lock(&lock_path));

        // Release and re-acquire should succeed
        release_lock(&lock_path);
        assert!(acquire_lock(&lock_path));

        // Cleanup
        release_lock(&lock_path);
    }
}
