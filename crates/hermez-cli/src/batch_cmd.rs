#![allow(dead_code)]
//! Batch processing subcommands.
//!
//! Exposes `hermez batch run` for parallel RL/data generation runs.

use std::path::Path;

use console::Style;

use hermez_batch::{BatchConfig, BatchRunner, list_distributions};

/// Options for running batch processing.
#[derive(Default)]
pub struct BatchRunOptions {
    pub dataset: String,
    pub run_name: Option<String>,
    pub model: Option<String>,
    pub batch_size: Option<usize>,
    pub workers: Option<usize>,
    pub max_iterations: Option<usize>,
    pub max_samples: Option<usize>,
    pub resume: bool,
    pub distribution: Option<String>,
}

/// Run batch processing on a JSONL dataset.
pub fn cmd_batch_run(opts: &BatchRunOptions) -> anyhow::Result<()> {
    let green = Style::new().green();
    let cyan = Style::new().cyan();
    let dim = Style::new().dim();

    // Validate dataset exists
    if !Path::new(&opts.dataset).exists() {
        eprintln!("Dataset file not found: {}", opts.dataset);
        return Ok(());
    }

    // Validate distribution if provided
    if let Some(ref dist) = opts.distribution {
        if !hermez_batch::validate_distribution(dist) {
            eprintln!("Unknown distribution: {dist}");
            eprintln!("Available distributions:");
            for (name, desc) in list_distributions() {
                eprintln!("  {name} — {desc}");
            }
            return Ok(());
        }
        println!("  {} Distribution: {}", cyan.apply_to("◆"), dist);
    }

    let config = BatchConfig {
        dataset_file: opts.dataset.clone(),
        run_name: opts.run_name.clone().unwrap_or_else(|| {
            Path::new(&opts.dataset)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "default".to_string())
        }),
        model: opts.model.clone().unwrap_or_else(|| "anthropic/claude-opus-4.6".to_string()),
        batch_size: opts.batch_size.unwrap_or(10),
        num_workers: opts.workers.unwrap_or(4),
        max_iterations: opts.max_iterations.unwrap_or(90),
        max_samples: opts.max_samples.unwrap_or(0),
        base_url: std::env::var("OPENAI_BASE_URL").ok(),
        api_key: std::env::var("OPENAI_API_KEY")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok()),
        output_dir: None,
    };

    println!();
    println!("{}", cyan.apply_to("◆ Batch Run"));
    println!("  Dataset:     {}", config.dataset_file);
    println!("  Run name:    {}", config.run_name);
    println!("  Model:       {}", config.model);
    println!("  Batch size:  {}", config.batch_size);
    println!("  Workers:     {}", config.num_workers);
    println!("  Max iters:   {}", config.max_iterations);
    if config.max_samples > 0 {
        println!("  Max samples: {}", config.max_samples);
    }
    if opts.resume {
        println!("  {}", dim.apply_to("Resume mode: skipping completed prompts"));
    }
    println!();

    let mut runner = BatchRunner::new(config)?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| {
        hermez_core::HermezError::new(hermez_core::ErrorCategory::InternalError, e.to_string())
    })?;

    let summary = rt.block_on(async {
        runner.run(opts.resume).await
    })?;

    println!();
    println!("  {} Batch run complete", green.apply_to("✓"));
    println!("  Total entries:    {}", summary.total_entries);
    println!("  Completed:        {}", summary.completed_entries);
    println!("  Batches:          {}", summary.total_batches);
    println!("  Output:           {}", summary.output_dir);
    println!();

    Ok(())
}

/// List available toolset distributions.
pub fn cmd_batch_distributions() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();

    println!();
    println!("{}", cyan.apply_to("◆ Toolset Distributions"));
    println!();

    let dists = list_distributions();
    for (name, desc) in &dists {
        println!("  {name:<20} {desc}");
    }
    println!();
    println!("  Use --distribution <name> with 'hermez batch run' to sample toolsets.");
    println!();

    Ok(())
}

/// Show status of a batch run (checkpoint, progress).
pub fn cmd_batch_status(run_name: &str) -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    let output_dir = format!("data/{run_name}");
    let checkpoint_path = Path::new(&output_dir).join("checkpoint.json");

    println!();
    println!("{}", cyan.apply_to("◆ Batch Status"));
    println!("  Run name: {run_name}");
    println!("  Output:   {output_dir}");
    println!();

    if !checkpoint_path.exists() {
        println!("  {} No checkpoint found — run has not started.", yellow.apply_to("→"));
        println!("    {}", dim.apply_to("Start with: hermez batch run <dataset.jsonl> --name <run_name>"));
        println!();
        return Ok(());
    }

    let checkpoint = hermez_batch::Checkpoint::load(&checkpoint_path)
        .ok()
        .flatten();

    match checkpoint {
        Some(cp) => {
            println!("  {} Checkpoint found", green.apply_to("✓"));
            println!("  Completed prompts: {}", cp.completed_prompts.len());
            println!("  Batches processed: {}", cp.batch_stats.len());

            let total_processed: usize = cp.batch_stats.iter().map(|s| s.processed).sum();
            let total_skipped: usize = cp.batch_stats.iter().map(|s| s.skipped).sum();
            println!("  Total processed:     {total_processed}");
            println!("  Total skipped:       {total_skipped}");

            if let Some(last) = cp.batch_stats.last() {
                println!("  Last batch:          #{}", last.batch_num);
            }

            let ts = chrono::DateTime::from_timestamp(cp.last_updated as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_string());
            println!("  Last updated:        {ts}");
        }
        None => {
            println!("  {} Checkpoint file exists but could not be parsed.", yellow.apply_to("⚠"));
        }
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_distributions_nonempty() {
        let dists = list_distributions();
        assert!(!dists.is_empty());
    }

    #[test]
    fn test_cmd_batch_run_missing_dataset() {
        // Should print error and return Ok
        let opts = BatchRunOptions {
            dataset: "/nonexistent/file.jsonl".to_string(),
            ..Default::default()
        };
        let result = cmd_batch_run(&opts);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cmd_batch_distributions() {
        let result = cmd_batch_distributions();
        assert!(result.is_ok());
    }
}
