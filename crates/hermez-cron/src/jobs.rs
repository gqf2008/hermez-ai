//! Cron job storage — JSON file with atomic writes, CRUD operations.
//!
//! Mirrors the Python `cron/jobs.py`.

use std::str::FromStr;
use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use hermez_core::{HermezError, Result};

/// Grace window for recovering missed one-shot jobs (seconds).
const ONESHOT_GRACE_SECONDS: i64 = 120;

/// Minimum grace window for recurring jobs (seconds).
const MIN_GRACE: i64 = 120;

/// Maximum grace window for recurring jobs (seconds).
const MAX_GRACE: i64 = 7200; // 2 hours

/// Compute grace seconds for a schedule — half the period, clamped.
fn compute_grace_seconds(schedule: &Schedule) -> i64 {
    match schedule {
        Schedule::Interval { minutes } => {
            let period = (*minutes as i64) * 60;
            let grace = period / 2;
            grace.clamp(MIN_GRACE, MAX_GRACE)
        }
        Schedule::Cron { expr } => {
            // Estimate period from cron expression
            if let Ok(parsed) = cron::Schedule::from_str(expr) {
                let now = Utc::now();
                let mut iter = parsed.after(&now);
                if let (Some(first), Some(second)) = (iter.next(), iter.next()) {
                    let period = (second - first).num_seconds();
                    return (period / 2).clamp(MIN_GRACE, MAX_GRACE);
                }
            }
            MIN_GRACE
        }
        Schedule::Once { .. } => ONESHOT_GRACE_SECONDS,
    }
}

/// Check if a one-shot job is still recoverable within grace window.
fn recoverable_oneshot_run_at(
    schedule: &Schedule,
    now: DateTime<Utc>,
    last_run_at: Option<&str>,
) -> Option<String> {
    // Only applies to one-shot jobs that haven't run yet
    if !matches!(schedule, Schedule::Once { .. }) || last_run_at.is_some() {
        return None;
    }

    if let Schedule::Once { run_at: Some(run_at) } = schedule {
        if let Ok(run_at_dt) = DateTime::parse_from_rfc3339(run_at) {
            let run_at_dt = run_at_dt.with_timezone(&Utc);
            // Within grace window?
            if (now - run_at_dt).num_seconds() <= ONESHOT_GRACE_SECONDS {
                return Some(run_at.clone());
            }
        }
    }

    None
}

/// A single cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique job ID (12-char hex UUID).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The prompt to run.
    pub prompt: String,
    /// Skill names to load.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Provider override.
    #[serde(default)]
    pub provider: Option<String>,
    /// Base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Pre-run script path.
    #[serde(default)]
    pub script: Option<String>,
    /// Parsed schedule.
    pub schedule: Schedule,
    /// Display string for the schedule.
    #[serde(default)]
    pub schedule_display: String,
    /// Repeat configuration.
    #[serde(default)]
    pub repeat: RepeatConfig,
    /// Whether the job is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Job state: "scheduled", "paused", "completed".
    #[serde(default)]
    pub state: String,
    /// When the job was paused.
    #[serde(default)]
    pub paused_at: Option<String>,
    /// Why the job was paused.
    #[serde(default)]
    pub paused_reason: Option<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: String,
    /// Next scheduled run time (ISO 8601).
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// Last run time (ISO 8601).
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// Last run status.
    #[serde(default)]
    pub last_status: Option<String>,
    /// Last run error.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Last delivery error.
    #[serde(default)]
    pub last_delivery_error: Option<String>,
    /// Delivery target: "local", "origin", "platform:target".
    #[serde(default)]
    pub deliver: String,
    /// Origin info (creation source).
    #[serde(default)]
    pub origin: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// Parsed schedule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// One-shot: run once after a duration or at a specific time.
    Once {
        /// ISO 8601 time for one-shot.
        run_at: Option<String>,
    },
    /// Recurring interval: "every 30m", "every 2h", etc.
    Interval {
        /// Minutes between runs.
        minutes: u64,
    },
    /// Standard cron expression: "0 9 * * *".
    Cron {
        /// Cron expression.
        expr: String,
    },
}

/// Repeat configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct RepeatConfig {
    /// Maximum number of runs (0 = unlimited).
    #[serde(default)]
    pub times: usize,
    /// Number of completed runs.
    #[serde(default)]
    pub completed: usize,
}


/// Job store — manages jobs in a JSON file.
pub struct JobStore {
    path: PathBuf,
    jobs: HashMap<String, CronJob>,
}

impl JobStore {
    /// Create a new job store, loading from disk.
    pub fn new() -> Result<Self> {
        let path = get_jobs_path();
        let parent = path.parent().expect("jobs path should have a parent directory");
        std::fs::create_dir_all(parent).map_err(|e| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Failed to create jobs directory: {e}"),
            )
        })?;

        let jobs = load_jobs(&path)?;
        Ok(Self { path, jobs })
    }

    /// Save all jobs to disk atomically.
    pub fn save(&self) -> Result<()> {
        let data = serde_json::json!({
            "jobs": self.jobs.values().collect::<Vec<_>>(),
            "updated_at": Utc::now().to_rfc3339(),
        });
        atomic_write_json(&self.path, &data)
    }

    /// Get a job by ID.
    pub fn get(&self, id: &str) -> Option<&CronJob> {
        self.jobs.get(id)
    }

    /// List all jobs.
    pub fn list(&self, include_disabled: bool) -> Vec<&CronJob> {
        self.jobs
            .values()
            .filter(|j| include_disabled || j.enabled)
            .collect()
    }

    /// Create a new job.
    pub fn create(&mut self, prompt: &str, schedule_str: &str, name: Option<&str>) -> Result<CronJob> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let schedule = parse_schedule(schedule_str).ok_or_else(|| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Invalid schedule expression: {schedule_str}"),
            )
        })?;

        let display = schedule_display(&schedule);
        let now = Utc::now().to_rfc3339();
        let next_run = compute_next_run(&schedule, None);

        let job = CronJob {
            id: id.clone(),
            name: name.unwrap_or(&id).to_string(),
            prompt: prompt.to_string(),
            skills: Vec::new(),
            model: None,
            provider: None,
            base_url: None,
            script: None,
            schedule,
            schedule_display: display,
            repeat: RepeatConfig::default(),
            enabled: true,
            state: "scheduled".to_string(),
            paused_at: None,
            paused_reason: None,
            created_at: now.clone(),
            next_run_at: next_run,
            last_run_at: None,
            last_status: None,
            last_error: None,
            last_delivery_error: None,
            deliver: "local".to_string(),
            origin: None,
        };

        self.jobs.insert(id, job.clone());
        self.save()?;
        Ok(job)
    }

    /// Update a job with new values.
    pub fn update(&mut self, id: &str, updates: JobUpdates) -> Result<()> {
        let job = self.jobs.get_mut(id).ok_or_else(|| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Job not found: {id}"),
            )
        })?;

        if let Some(prompt) = updates.prompt {
            job.prompt = prompt;
        }
        if let Some(name) = updates.name {
            job.name = name;
        }
        if let Some(model) = updates.model {
            job.model = Some(model);
        }
        if let Some(schedule_str) = updates.schedule {
            if let Some(schedule) = parse_schedule(&schedule_str) {
                job.schedule_display = schedule_display(&schedule);
                job.schedule = schedule;
                job.next_run_at = compute_next_run(&job.schedule, None);
            }
        }
        if let Some(deliver) = updates.deliver {
            job.deliver = deliver;
        }
        if let Some(enabled) = updates.enabled {
            job.enabled = enabled;
        }
        if let Some(skills) = updates.skills {
            job.skills = skills;
        }
        if let Some(next_run) = updates.next_run_override {
            job.next_run_at = Some(next_run);
        }

        self.save()
    }

    /// Pause a job.
    pub fn pause(&mut self, id: &str, reason: Option<&str>) -> Result<()> {
        let job = self.jobs.get_mut(id).ok_or_else(|| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Job not found: {id}"),
            )
        })?;
        job.enabled = false;
        job.state = "paused".to_string();
        job.paused_at = Some(Utc::now().to_rfc3339());
        job.paused_reason = reason.map(String::from);
        self.save()
    }

    /// Resume a paused job.
    pub fn resume(&mut self, id: &str) -> Result<()> {
        let job = self.jobs.get_mut(id).ok_or_else(|| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Job not found: {id}"),
            )
        })?;
        job.enabled = true;
        job.state = "scheduled".to_string();
        job.paused_at = None;
        job.paused_reason = None;
        if job.next_run_at.is_none() {
            job.next_run_at = compute_next_run(&job.schedule, None);
        }
        self.save()
    }

    /// Delete a job.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        if self.jobs.remove(id).is_some() {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Mark a job as run.
    ///
    /// Logs a warning and returns an error when the job is not found
    /// (mirrors Python: warning + skip save instead of silent no-op).
    pub fn mark_run(&mut self, id: &str, success: bool, error: Option<&str>, delivery_error: Option<&str>) -> Result<()> {
        let job = self.jobs.get_mut(id).ok_or_else(|| {
            tracing::warn!("mark_job_run: job_id {id} not found, skipping save");
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Job not found: {id}"),
            )
        })?;
        job.last_run_at = Some(Utc::now().to_rfc3339());
        job.last_status = Some(if success { "success" } else { "failed" }.to_string());
        job.last_error = error.map(String::from);
        job.last_delivery_error = delivery_error.map(String::from);
        job.repeat.completed += 1;

        if job.repeat.times > 0 && job.repeat.completed >= job.repeat.times {
            job.state = "completed".to_string();
            job.enabled = false;
        }

        self.save()
    }

    /// Advance the next run time (preemptively, for crash safety).
    pub fn advance_next_run(&mut self, id: &str) -> Result<()> {
        let job = self.jobs.get_mut(id).ok_or_else(|| {
            HermezError::new(
                hermez_core::errors::ErrorCategory::InternalError,
                format!("Job not found: {id}"),
            )
        })?;

        let last_run = job.last_run_at.as_ref().map(|s| {
            match DateTime::parse_from_rfc3339(s) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => {
                    tracing::warn!("Corrupt last_run_at timestamp for job '{id}', using None: {s}");
                    DateTime::parse_from_rfc3339(&Utc::now().to_rfc3339())
                        .unwrap_or_else(|_| Utc::now().fixed_offset())
                        .with_timezone(&Utc)
                }
            }
        });
        job.next_run_at = compute_next_run(&job.schedule, last_run);
        self.save()
    }

    /// Get jobs that are due to run now.
    pub fn get_due_jobs(&self) -> Vec<&CronJob> {
        let now = Utc::now();
        let mut due = Vec::new();

        for job in self.jobs.values() {
            if !job.enabled || job.state == "paused" || job.state == "completed" {
                continue;
            }

            let Some(next) = &job.next_run_at else {
                // One-shot with no next_run — check recoverable grace
                if let Some(_recovered) = recoverable_oneshot_run_at(&job.schedule, now, job.last_run_at.as_deref()) {
                    // Job is still within grace window, treat as due
                    due.push(job);
                }
                continue;
            };

            let Ok(next_dt) = DateTime::parse_from_rfc3339(next) else {
                continue;
            };
            let next_dt = next_dt.with_timezone(&Utc);

            if next_dt <= now {
                // For recurring jobs, check if past grace window (stale miss)
                let grace = compute_grace_seconds(&job.schedule);
                let is_recurring = matches!(job.schedule, Schedule::Interval { .. } | Schedule::Cron { .. });
                let elapsed = (now - next_dt).num_seconds();

                if is_recurring && elapsed > grace {
                    // Missed the grace window — fast-forward to next occurrence
                    // Don't fire this run, just skip it
                    continue;
                }

                due.push(job);
            }
        }

        due
    }

    /// Get jobs that are due to run now, and return a list of jobs that
    /// need to be fast-forwarded (missed grace).
    pub fn get_due_jobs_with_fast_forward(&mut self) -> (Vec<&CronJob>, Vec<String>) {
        let now = Utc::now();
        let mut due = Vec::new();
        let mut fast_forwarded = Vec::new();

        // Clone job IDs to avoid borrow issues
        let job_ids: Vec<String> = self.jobs.keys().cloned().collect();

        for id in job_ids {
            let Some(job) = self.jobs.get(&id) else {
                continue;
            };
            if !job.enabled || job.state == "paused" || job.state == "completed" {
                continue;
            }

            let Some(next) = &job.next_run_at else {
                // One-shot with no next_run — check recoverable grace
                if let Some(recovered) = recoverable_oneshot_run_at(&job.schedule, now, job.last_run_at.as_deref()) {
                    // Update in storage
                    if let Some(job_mut) = self.jobs.get_mut(&id) {
                        job_mut.next_run_at = Some(recovered.clone());
                    }
                    due.push(id);
                }
                continue;
            };

            let Ok(next_dt) = DateTime::parse_from_rfc3339(next) else {
                continue;
            };
            let next_dt = next_dt.with_timezone(&Utc);

            if next_dt <= now {
                // For recurring jobs, check if past grace window (stale miss)
                let grace = compute_grace_seconds(&job.schedule);
                let is_recurring = matches!(job.schedule, Schedule::Interval { .. } | Schedule::Cron { .. });
                let elapsed = (now - next_dt).num_seconds();

                if is_recurring && elapsed > grace {
                    // Fast-forward to next occurrence
                    if let Some(new_next) = compute_next_run(&job.schedule, Some(now)) {
                        tracing::info!(
                            "Job '{}' missed scheduled time ({}, grace={}). Fast-forwarding to {}",
                            job.name, next, grace, new_next
                        );
                        if let Some(job_mut) = self.jobs.get_mut(&id) {
                            job_mut.next_run_at = Some(new_next);
                        }
                        fast_forwarded.push(id.clone());
                    }
                    continue;
                }

                due.push(id);
            }
        }

        // Save if any fast-forward happened
        if !fast_forwarded.is_empty() {
            let _ = self.save();
        }

        // Convert IDs back to references
        let due_refs = due.iter().filter_map(|id| self.jobs.get(id)).collect();
        (due_refs, fast_forwarded)
    }
}

/// Partial updates to a job.
pub struct JobUpdates {
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub schedule: Option<String>,
    pub deliver: Option<String>,
    pub enabled: Option<bool>,
    pub skills: Option<Vec<String>>,
    /// Override next_run_at directly (used by trigger_job).
    pub next_run_override: Option<String>,
}

/// Load jobs from a JSON file.
///
/// Raises an error on corrupted/unparseable JSON instead of silently
/// returning an empty list (mirrors Python: RuntimeError on corruption).
fn load_jobs(path: &PathBuf) -> Result<HashMap<String, CronJob>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let data = std::fs::read_to_string(path).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to read jobs file: {e}"),
        )
    })?;

    let parsed: serde_json::Value = serde_json::from_str(&data).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Cron database corrupted and unrepairable: {e}"),
        )
    })?;

    let jobs_array = parsed.get("jobs").and_then(|v| v.as_array()).cloned();
    let Some(jobs_array) = jobs_array else {
        return Ok(HashMap::new());
    };

    let mut jobs = HashMap::new();
    for job_val in jobs_array {
        if let Ok(job) = serde_json::from_value::<CronJob>(job_val) {
            jobs.insert(job.id.clone(), job);
        }
    }

    Ok(jobs)
}

/// Atomically write JSON data to a file.
fn atomic_write_json(path: &PathBuf, data: &serde_json::Value) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    let data_str = serde_json::to_string_pretty(data).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to serialize jobs: {e}"),
        )
    })?;
    std::fs::write(&temp_path, &data_str).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to write jobs file: {e}"),
        )
    })?;
    std::fs::rename(&temp_path, path).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to rename jobs file: {e}"),
        )
    })
}

/// Get the default jobs file path.
fn get_jobs_path() -> PathBuf {
    // Allow test isolation via env var
    if let Ok(base) = std::env::var("HERMEZ_CRON_TEST_DIR") {
        return PathBuf::from(base).join("cron").join("jobs.json");
    }
    let home = hermez_core::get_hermez_home();
    // Resolve symlinks and normalize path (mirrors Python .resolve())
    home.canonicalize().unwrap_or(home).join("cron").join("jobs.json")
}

/// Parse a schedule string into a Schedule enum.
///
/// Supported formats:
/// - "2026-02-03T14:00" → Once { run_at: ... }
/// - "30m", "2h", "1d" → Once { run_at: now + duration }
/// - "every 30m", "every 2h" → Interval { minutes }
/// - "0 9 * * *" → Cron { expr }
pub fn parse_schedule(s: &str) -> Option<Schedule> {
    let s = s.trim();

    // One-shot at specific time
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(Schedule::Once {
            run_at: Some(dt.with_timezone(&Utc).to_rfc3339()),
        });
    }

    // Interval: "every 30m", "every 2h", "every 1d"
    if let Some(rest) = s.strip_prefix("every ") {
        return parse_duration(rest.trim()).map(|minutes| Schedule::Interval { minutes });
    }

    // One-shot duration: "30m", "2h", "1d"
    if let Some(minutes) = parse_duration(s) {
        let run_at = Utc::now() + chrono::Duration::minutes(minutes as i64);
        return Some(Schedule::Once {
            run_at: Some(run_at.to_rfc3339()),
        });
    }

    // Cron expression: "0 9 * * *"
    if is_valid_cron(s) {
        return Some(Schedule::Cron {
            expr: s.to_string(),
        });
    }

    None
}

/// Parse a duration string: "30m", "2h", "1d" → minutes.
/// Supports aliases: min/minute/minutes, hr/hrs/hour/hours, day/days.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Single-char suffix (fast path)
    if let Some(minutes) = parse_duration_suffix(s) {
        return Some(minutes);
    }

    // Multi-char aliases
    let lower = s.to_lowercase();
    if let Some(num_str) = lower.strip_suffix("min")
        .or_else(|| lower.strip_suffix("minute"))
        .or_else(|| lower.strip_suffix("minutes"))
    {
        return num_str.parse::<u64>().ok();
    }
    if let Some(num_str) = lower.strip_suffix("hr")
        .or_else(|| lower.strip_suffix("hrs"))
        .or_else(|| lower.strip_suffix("hour"))
        .or_else(|| lower.strip_suffix("hours"))
    {
        return num_str.parse::<u64>().ok().map(|n| n * 60);
    }
    if let Some(num_str) = lower.strip_suffix("day")
        .or_else(|| lower.strip_suffix("days"))
    {
        return num_str.parse::<u64>().ok().map(|n| n * 24 * 60);
    }

    None
}

/// Parse duration with single-char suffix: "30m", "2h", "1d" → minutes.
fn parse_duration_suffix(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let last_char = s.chars().last()?;
    let num_str = &s[..s.len() - 1];
    let num: u64 = num_str.parse().ok()?;

    match last_char {
        'm' => Some(num),
        'h' => Some(num * 60),
        'd' => Some(num * 24 * 60),
        _ => None,
    }
}

/// Check if a string is a valid cron expression (5 fields).
fn is_valid_cron(s: &str) -> bool {
    let parts: Vec<&str> = s.split_whitespace().collect();
    parts.len() == 5
        && parts.iter().all(|p| {
            *p == "*"
                || p.contains(',')
                || p.contains('/')
                || p.contains('-')
                || p.parse::<u32>().is_ok()
        })
}

/// Get a human-readable display string for a schedule.
fn schedule_display(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Once { run_at } => {
            if let Some(dt) = run_at {
                format!("once at {dt}")
            } else {
                "once".to_string()
            }
        }
        Schedule::Interval { minutes } => {
            if *minutes >= 1440 {
                format!("every {}d", minutes / 1440)
            } else if *minutes >= 60 {
                format!("every {}h", minutes / 60)
            } else {
                format!("every {}m", minutes)
            }
        }
        Schedule::Cron { expr } => format!("cron: {expr}"),
    }
}

/// Compute the next run time for a schedule.
pub fn compute_next_run(schedule: &Schedule, last_run: Option<DateTime<Utc>>) -> Option<String> {
    match schedule {
        Schedule::Once { .. } => None,
        Schedule::Interval { minutes } => {
            let base = last_run.unwrap_or_else(Utc::now);
            let next = base + chrono::Duration::minutes(*minutes as i64);
            Some(next.to_rfc3339())
        }
        Schedule::Cron { expr } => {
            if let Ok(schedule_expr) = cron::Schedule::from_str(expr) {
                let after = last_run.unwrap_or_else(Utc::now);
                let next = schedule_expr.after(&after).next();
                next.map(|dt| dt.to_rfc3339())
            } else {
                None
            }
        }
    }
}

/// Get the output directory for a job.
pub fn get_output_dir(job_id: &str) -> PathBuf {
    let home = hermez_core::get_hermez_home();
    home.join("cron").join("output").join(job_id)
}

/// Save job output to a file.
pub fn save_job_output(job_id: &str, output: &str) -> Result<()> {
    let dir = get_output_dir(job_id);
    std::fs::create_dir_all(&dir).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to create output directory: {e}"),
        )
    })?;

    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let filename = format!("{timestamp}.md");
    let path = dir.join(&filename);

    std::fs::write(&path, output).map_err(|e| {
        HermezError::new(
            hermez_core::errors::ErrorCategory::InternalError,
            format!("Failed to write job output: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_schedule_once_time() {
        let s = parse_schedule("2026-02-03T14:00:00Z");
        assert!(matches!(s, Some(Schedule::Once { .. })));
    }

    #[test]
    fn test_parse_schedule_interval() {
        let s = parse_schedule("every 30m");
        match s {
            Some(Schedule::Interval { minutes }) => assert_eq!(minutes, 30),
            _ => panic!("Expected Interval, got {s:?}"),
        }
    }

    #[test]
    fn test_parse_schedule_cron() {
        let s = parse_schedule("0 9 * * *");
        match s {
            Some(Schedule::Cron { expr }) => assert_eq!(expr, "0 9 * * *"),
            _ => panic!("Expected Cron, got {s:?}"),
        }
    }

    #[test]
    fn test_parse_schedule_once_duration() {
        let s = parse_schedule("2h");
        assert!(matches!(s, Some(Schedule::Once { .. })));
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30m"), Some(30));
        assert_eq!(parse_duration("2h"), Some(120));
        assert_eq!(parse_duration("1d"), Some(1440));
        assert_eq!(parse_duration("invalid"), None);
    }

    #[test]
    fn test_schedule_display() {
        assert_eq!(schedule_display(&Schedule::Interval { minutes: 30 }), "every 30m");
        assert_eq!(schedule_display(&Schedule::Interval { minutes: 120 }), "every 2h");
        assert_eq!(schedule_display(&Schedule::Interval { minutes: 2880 }), "every 2d");
        assert_eq!(
            schedule_display(&Schedule::Cron { expr: "0 9 * * *".to_string() }),
            "cron: 0 9 * * *"
        );
    }

    #[test]
    fn test_compute_next_run_interval() {
        let schedule = Schedule::Interval { minutes: 60 };
        let next = compute_next_run(&schedule, None);
        assert!(next.is_some());
        let next = DateTime::parse_from_rfc3339(&next.unwrap()).unwrap();
        let now = Utc::now().with_timezone(&next.timezone());
        assert!(next >= now);
    }

    #[test]
    fn test_compute_next_run_once() {
        let schedule = Schedule::Once {
            run_at: Some(Utc::now().to_rfc3339()),
        };
        let next = compute_next_run(&schedule, None);
        assert!(next.is_none());
    }

    #[test]
    fn test_job_store_crud() {
        // Use isolated test directory
        let test_dir = std::env::temp_dir().join("hermez_cron_test_crud");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        std::env::set_var("HERMEZ_CRON_TEST_DIR", test_dir.to_str().unwrap());

        let mut store = JobStore::new().unwrap();
        let initial_count = store.list(true).len();

        let job = store.create("Test prompt", "every 5m", Some("test_job")).unwrap();
        assert_eq!(job.name, "test_job");
        assert_eq!(job.state, "scheduled");
        assert!(job.enabled);

        assert_eq!(store.list(true).len(), initial_count + 1);

        store.pause(&job.id, Some("testing")).unwrap();
        assert!(!store.get(&job.id).unwrap().enabled);

        store.resume(&job.id).unwrap();
        assert!(store.get(&job.id).unwrap().enabled);

        let removed = store.remove(&job.id).unwrap();
        assert!(removed);
        assert_eq!(store.list(true).len(), initial_count);

        std::env::remove_var("HERMEZ_CRON_TEST_DIR");
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_load_jobs_corrupted_json() {
        let test_dir = std::env::temp_dir().join("hermez_cron_test_corrupted");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        let jobs_file = test_dir.join("cron").join("jobs.json");
        std::fs::create_dir_all(jobs_file.parent().unwrap()).unwrap();

        // Write corrupted JSON
        std::fs::write(&jobs_file, "{ this is not valid json }}}").unwrap();

        std::env::set_var("HERMEZ_CRON_TEST_DIR", test_dir.to_str().unwrap());
        let result = JobStore::new();
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Cron database corrupted"));

        std::env::remove_var("HERMEZ_CRON_TEST_DIR");
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_mark_run_job_not_found() {
        let test_dir = std::env::temp_dir().join("hermez_cron_test_mark_run");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        std::env::set_var("HERMEZ_CRON_TEST_DIR", test_dir.to_str().unwrap());

        let mut store = JobStore::new().unwrap();
        let err = store.mark_run("nonexistent_id", true, None, None).unwrap_err();
        assert!(err.to_string().contains("Job not found"));

        std::env::remove_var("HERMEZ_CRON_TEST_DIR");
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_mark_run_success() {
        let test_dir = std::env::temp_dir().join("hermez_cron_test_mark_success");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        std::env::set_var("HERMEZ_CRON_TEST_DIR", test_dir.to_str().unwrap());

        let mut store = JobStore::new().unwrap();
        let job = store.create("Test prompt", "every 5m", None).unwrap();
        store.mark_run(&job.id, true, None, None).unwrap();

        let updated = store.get(&job.id).unwrap();
        assert!(updated.last_run_at.is_some());
        assert_eq!(updated.last_status, Some("success".to_string()));
        assert_eq!(updated.repeat.completed, 1);

        std::env::remove_var("HERMEZ_CRON_TEST_DIR");
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_mark_run_completes_repeat_limit() {
        let test_dir = std::env::temp_dir().join("hermez_cron_test_repeat_limit");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        std::env::set_var("HERMEZ_CRON_TEST_DIR", test_dir.to_str().unwrap());

        let mut store = JobStore::new().unwrap();
        let job = store.create("Test prompt", "every 5m", None).unwrap();

        // Set repeat limit to 2
        let job_id = job.id.clone();
        store.jobs.get_mut(&job_id).unwrap().repeat.times = 2;
        store.save().unwrap();

        // First run
        store.mark_run(&job_id, true, None, None).unwrap();
        let j = store.get(&job_id).unwrap();
        assert_eq!(j.repeat.completed, 1);
        assert_eq!(j.state, "scheduled");

        // Second run — should complete
        store.mark_run(&job_id, true, None, None).unwrap();
        let j = store.get(&job_id).unwrap();
        assert_eq!(j.repeat.completed, 2);
        assert_eq!(j.state, "completed");
        assert!(!j.enabled);

        std::env::remove_var("HERMEZ_CRON_TEST_DIR");
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_compute_grace_seconds_interval() {
        // 30min interval → grace = 15min = 900s
        let schedule = Schedule::Interval { minutes: 30 };
        let grace = compute_grace_seconds(&schedule);
        assert_eq!(grace, 900);

        // 5min interval → grace = 2.5min = 150s (above MIN_GRACE=120)
        let schedule = Schedule::Interval { minutes: 5 };
        let grace = compute_grace_seconds(&schedule);
        assert_eq!(grace, 150);

        // 1min interval → grace = 30s → clamped to MIN_GRACE=120
        let schedule = Schedule::Interval { minutes: 1 };
        let grace = compute_grace_seconds(&schedule);
        assert_eq!(grace, 120);

        // Daily interval → grace = 12h = 43200s → clamped to MAX_GRACE=7200
        let schedule = Schedule::Interval { minutes: 1440 };
        let grace = compute_grace_seconds(&schedule);
        assert_eq!(grace, 7200);
    }

    #[test]
    fn test_recoverable_oneshot_run_at() {
        let now = Utc::now();

        // One-shot within grace window → recoverable
        let schedule = Schedule::Once {
            run_at: Some((now - chrono::Duration::seconds(60)).to_rfc3339()),
        };
        let result = recoverable_oneshot_run_at(&schedule, now, None);
        assert!(result.is_some());

        // One-shot past grace window → not recoverable
        let schedule = Schedule::Once {
            run_at: Some((now - chrono::Duration::seconds(300)).to_rfc3339()),
        };
        let result = recoverable_oneshot_run_at(&schedule, now, None);
        assert!(result.is_none());

        // One-shot already run → not recoverable
        let schedule = Schedule::Once {
            run_at: Some((now - chrono::Duration::seconds(60)).to_rfc3339()),
        };
        let result = recoverable_oneshot_run_at(&schedule, now, Some("2026-01-01T00:00:00Z"));
        assert!(result.is_none());

        // Non-one-shot → not recoverable
        let schedule = Schedule::Interval { minutes: 30 };
        let result = recoverable_oneshot_run_at(&schedule, now, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_duration_aliases() {
        // Single-char
        assert_eq!(parse_duration("30m"), Some(30));
        assert_eq!(parse_duration("2h"), Some(120));
        assert_eq!(parse_duration("1d"), Some(1440));
        // Multi-char aliases
        assert_eq!(parse_duration("30min"), Some(30));
        assert_eq!(parse_duration("1minute"), Some(1));
        assert_eq!(parse_duration("5minutes"), Some(5));
        assert_eq!(parse_duration("2hr"), Some(120));
        assert_eq!(parse_duration("1hour"), Some(60));
        assert_eq!(parse_duration("3hours"), Some(180));
        assert_eq!(parse_duration("1day"), Some(1440));
        assert_eq!(parse_duration("2days"), Some(2880));
        // Invalid
        assert_eq!(parse_duration("invalid"), None);
    }
}
