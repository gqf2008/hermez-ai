//! # Hermez Cron
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Scheduled job execution with cron expressions.
//! Mirrors the Python `cron/` directory.

pub mod delivery;
pub mod jobs;
pub mod scheduler;

pub use delivery::{DeliveryTarget, deliver_result, resolve_delivery_target};
pub use jobs::{CronJob, JobStore, JobUpdates, parse_schedule, compute_next_run, save_job_output};
pub use scheduler::{run_scheduler, tick_once, trigger_job};
