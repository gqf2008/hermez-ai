//! # Hermez Batch
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Parallel batch processing with checkpoint/resume.
//! Mirrors the Python `batch_runner.py`.

pub mod checkpoint;
pub mod distributions;
pub mod runner;
pub mod trajectories;

pub use checkpoint::{BatchStat, Checkpoint};
pub use distributions::{
    all_possible_tools, list_distributions, normalize_tool_stats,
    sample_toolsets, validate_distribution,
};
pub use runner::{BatchConfig, BatchRunner, PromptEntry, RunSummary};
pub use trajectories::{
    TrajectoryEntry, TrajectoryMessage, ToolStats, ReasoningStats,
    extract_tool_stats, extract_reasoning_stats, combine_batch_files,
    convert_scratchpad_to_think, has_incomplete_scratchpad,
};
