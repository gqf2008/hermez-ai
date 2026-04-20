//! Integration tests for hermes-core configuration.

#[test]
fn test_hermes_home_resolution() {
    let home = hermes_core::get_hermes_home();
    assert!(home.exists() || home.to_string_lossy().contains(".hermes"));
}

#[test]
fn test_config_load_defaults() {
    let config = hermes_core::HermesConfig::default();
    assert_eq!(config.model.name, Some("anthropic/claude-opus-4-6".to_string()));
}

#[test]
fn test_redact_sensitive_text() {
    let text = "API key: sk-abc123secret";
    let redacted = hermes_core::redact_sensitive_text(text);
    assert!(!redacted.contains("sk-abc123secret"));
}
