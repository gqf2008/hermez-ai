//! Integration tests for hermez-core configuration.

#[test]
fn test_hermez_home_resolution() {
    let home = hermez_core::get_hermez_home();
    assert!(home.exists() || home.to_string_lossy().contains(".hermez"));
}

#[test]
fn test_config_load_defaults() {
    let config = hermez_core::HermezConfig::default();
    assert_eq!(config.model.name, Some("anthropic/claude-opus-4-6".to_string()));
}

#[test]
fn test_redact_sensitive_text() {
    let text = "API key: sk-abc123secret";
    let redacted = hermez_core::redact_sensitive_text(text);
    assert!(!redacted.contains("sk-abc123secret"));
}
