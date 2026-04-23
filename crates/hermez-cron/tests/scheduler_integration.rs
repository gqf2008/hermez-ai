//! Integration tests for hermez-cron scheduler and jobs.

#[test]
fn test_cron_expression_parse() {
    let schedule = hermez_cron::jobs::parse_schedule("0 9 * * *");
    assert!(schedule.is_some());
}

#[test]
fn test_job_store_create_and_list() {
    let tmp = std::env::temp_dir().join("hermez_cron_test_jobs.json");
    let mut store = hermez_cron::jobs::JobStore::new().unwrap();

    let job = store.create("echo hello", "0 * * * *", Some("Test Job")).unwrap();
    assert_eq!(job.name, "Test Job");

    let jobs = store.list(true);
    assert!(!jobs.is_empty());

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_compute_next_run_interval() {
    use chrono::Utc;
    let schedule = hermez_cron::jobs::Schedule::Interval { minutes: 30 };
    let next = hermez_cron::jobs::compute_next_run(&schedule, Some(Utc::now()));
    assert!(next.is_some());
}
