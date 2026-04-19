#![allow(dead_code)]
//! Cron management subcommands.
//!
//! Mirrors Python: hermes cron list/create/pause/resume/delete/edit/run/status/tick

use console::Style;

use hermes_cron::jobs::{JobStore, JobUpdates};

fn get_store() -> Option<JobStore> {
    JobStore::new().ok()
}

/// List all cron jobs.
pub fn cmd_cron_list(all: bool) -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let dim = Style::new().dim();
    let yellow = Style::new().yellow();

    let store = match get_store() {
        Some(s) => s,
        None => {
            println!();
            println!("{}", cyan.apply_to("◆ Scheduled Jobs"));
            println!();
            println!("  {}", dim.apply_to("No cron jobs scheduled."));
            println!("  Create one with: hermes cron create");
            println!();
            return Ok(());
        }
    };

    let jobs = store.list(!all);

    println!();
    println!("{}", cyan.apply_to("◆ Scheduled Jobs"));
    println!();

    if jobs.is_empty() {
        println!("  {}", dim.apply_to("No cron jobs scheduled."));
        println!("  Create one with: hermes cron create");
        println!();
        return Ok(());
    }

    println!("{:<14} {:<20} {:<20} {:<10} {:<20}", "ID", "Name", "Schedule", "Status", "Next Run");
    println!("{}", "-".repeat(90));

    for job in &jobs {
        let status = if job.enabled {
            green.apply_to("active").to_string()
        } else {
            yellow.apply_to("paused").to_string()
        };
        let next = job.next_run_at.as_deref().unwrap_or("unknown").to_string();

        println!("{:<14} {:<20} {:<20} {:<10} {}", job.id, job.name, job.schedule_display, status, next);
    }
    println!();
    println!("  Total: {} job(s)", jobs.len());
    println!();

    Ok(())
}

/// Create a new cron job.
pub fn cmd_cron_create(
    name: &str,
    schedule: &str,
    command: &str,
    _prompt: Option<&str>,
    delivery: &str,
    enabled: bool,
    _repeat: usize,
    _skill: Option<&str>,
    _script: Option<&str>,
) -> anyhow::Result<()> {
    let green = Style::new().green();
    let cyan = Style::new().cyan();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    let job = store.create(command, schedule, Some(name))?;
    if !enabled {
        store.pause(&job.id, Some("created paused"))?;
    }

    println!();
    println!("{}", cyan.apply_to("◆ Cron Job Created"));
    println!("  {} Job '{name}' created with ID: {}", green.apply_to("✓"), job.id);
    println!("  Schedule:   {}", job.schedule_display);
    println!("  Command:    {command}");
    println!("  Delivery:   {delivery}");
    println!("  Enabled:    {enabled}");
    println!();
    println!("  Start the scheduler with: hermes cron start");
    println!();

    Ok(())
}

/// Delete a cron job.
pub fn cmd_cron_delete(job_id: &str, _force: bool) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    if store.get(job_id).is_some() {
        store.remove(job_id)?;
        println!("  {} Job '{job_id}' deleted.", green.apply_to("✓"));
    } else {
        println!("  {} Job '{job_id}' not found.", yellow.apply_to("✗"));
    }
    println!();

    Ok(())
}

/// Pause a cron job.
pub fn cmd_cron_pause(job_id: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    if store.get(job_id).is_some() {
        store.pause(job_id, None)?;
        println!("  {} Job '{job_id}' paused.", green.apply_to("✓"));
    } else {
        println!("  {} Job '{job_id}' not found.", yellow.apply_to("✗"));
    }
    println!();

    Ok(())
}

/// Resume a cron job.
pub fn cmd_cron_resume(job_id: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    if let Some(_job) = store.get(job_id) {
        store.resume(job_id)?;
        println!("  {} Job '{job_id}' resumed.", green.apply_to("✓"));
    } else {
        println!("  {} Job '{job_id}' not found.", yellow.apply_to("✗"));
    }
    println!();

    Ok(())
}

/// Edit a cron job's properties.
pub fn cmd_cron_edit(
    job_id: &str,
    schedule: Option<&str>,
    name: Option<&str>,
    prompt: Option<&str>,
    deliver: Option<&str>,
    _repeat: Option<usize>,
    _script: Option<&str>,
    _skill: Option<&str>,
    _add_skill: Option<&str>,
    _remove_skill: Option<&str>,
    _clear_skills: bool,
) -> anyhow::Result<()> {
    let green = Style::new().green();
    let cyan = Style::new().cyan();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    let job = store.get(job_id).ok_or_else(|| anyhow::anyhow!("Job '{}' not found", job_id))?;
    let existing = job.clone();

    let updates = JobUpdates {
        schedule: schedule.map(|s| s.to_string()),
        name: name.map(|s| s.to_string()),
        prompt: prompt.map(|s| s.to_string()),
        deliver: deliver.map(|s| s.to_string()),
        enabled: None,
        skills: None,
        model: None,
        next_run_override: None,
    };

    store.update(job_id, updates)?;

    println!();
    println!("{}", cyan.apply_to("◆ Cron Job Updated"));
    println!("  {} Job '{}' updated", green.apply_to("✓"), job_id);

    if let Some(ref s) = schedule {
        println!("  Schedule:   {s}");
    } else {
        println!("  Schedule:   {}", existing.schedule_display);
    }
    if let Some(ref n) = name {
        println!("  Name:       {n}");
    } else {
        println!("  Name:       {}", existing.name);
    }
    if let Some(p) = prompt {
        println!("  Prompt:     {}", p.chars().take(60).collect::<String>());
    }
    if let Some(ref d) = deliver {
        println!("  Delivery:   {d}");
    }
    println!();

    Ok(())
}

/// Trigger a cron job to run on the next scheduler tick.
pub fn cmd_cron_run(job_id: &str) -> anyhow::Result<()> {
    let green = Style::new().green();

    let mut store = get_store().ok_or_else(|| anyhow::anyhow!("Failed to initialize cron store"))?;

    let job = store.get(job_id).ok_or_else(|| anyhow::anyhow!("Job '{}' not found", job_id))?;
    let job_name = job.name.clone();

    // Mark the job so scheduler picks it up on next tick
    store.mark_run(job_id, false, None, None)?;

    println!();
    println!("  {} Job '{}' ({}) triggered for next run", green.apply_to("✓"), job_id, job_name);
    println!("  The scheduler will pick it up on the next tick cycle.");
    println!();

    Ok(())
}

/// Show cron scheduler status.
pub fn cmd_cron_status() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    let store = match get_store() {
        Some(s) => s,
        None => {
            println!();
            println!("{}", cyan.apply_to("◆ Cron Scheduler Status"));
            println!();
            println!("  {}", dim.apply_to("No cron store found."));
            println!();
            return Ok(());
        }
    };

    let jobs = store.list(false);
    let active_count = jobs.iter().filter(|j| j.enabled).count();
    let earliest_next = jobs
        .iter()
        .filter(|j| j.enabled)
        .filter_map(|j| j.next_run_at.as_ref())
        .min()
        .map(|s| s.as_str())
        .unwrap_or("N/A");

    println!();
    println!("{}", cyan.apply_to("◆ Cron Scheduler Status"));
    println!();
    println!("  Jobs: {} total, {} active", jobs.len(), active_count);
    println!("  Earliest next run: {earliest_next}");
    println!();
    println!("  {}", yellow.apply_to("Note: Jobs fire when the gateway scheduler is running."));
    println!("  Start the gateway with: hermes gateway start");
    println!();

    Ok(())
}

/// Run all due cron jobs once (for debugging/manual triggering).
pub fn cmd_cron_tick() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    let store = match get_store() {
        Some(s) => s,
        None => {
            println!();
            println!("{}", cyan.apply_to("◆ Cron Tick"));
            println!();
            println!("  {}", dim.apply_to("No cron store found. No jobs to run."));
            println!();
            return Ok(());
        }
    };

    let due = store.get_due_jobs();
    if due.is_empty() {
        println!();
        println!("{}", cyan.apply_to("◆ Cron Tick"));
        println!();
        println!("  {}", dim.apply_to("No jobs due at this time."));
        println!();
        return Ok(());
    }

    println!();
    println!("{}", cyan.apply_to("◆ Cron Tick"));
    println!();
    println!("  {} {} job(s) due:", green.apply_to("✓"), due.len());

    for job in &due {
        println!("    - {} ({})", job.name, job.id);
    }
    println!();
    println!("  {}", yellow.apply_to("Note: This only lists due jobs. Actual execution requires the gateway scheduler."));
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_store_returns_some() {
        // JobStore::new() creates a directory, so it should succeed
        let result = get_store();
        assert!(result.is_some());
    }
}
