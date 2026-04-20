//! Integration tests for hermes-batch runner.

#[test]
fn test_distributions_list_not_empty() {
    let dists = hermes_batch::distributions::list_distributions();
    assert!(!dists.is_empty());
}

#[test]
fn test_checkpoint_roundtrip() {
    let tmp_path = std::env::temp_dir().join("checkpoint_test.json");

    let mut checkpoint = hermes_batch::checkpoint::Checkpoint::new("test-run");
    checkpoint.completed_prompts.push("prompt-1".into());
    checkpoint.completed_prompts.push("prompt-2".into());

    checkpoint.save(&tmp_path).unwrap();
    let loaded = hermes_batch::checkpoint::Checkpoint::load(&tmp_path).unwrap().unwrap();
    assert_eq!(loaded.completed_prompts.len(), 2);
    assert_eq!(loaded.run_name, "test-run");

    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn test_validate_distribution() {
    assert!(hermes_batch::distributions::validate_distribution("default"));
    assert!(!hermes_batch::distributions::validate_distribution("nonexistent"));
}
