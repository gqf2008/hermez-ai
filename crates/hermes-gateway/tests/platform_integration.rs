//! Integration tests for hermes-gateway platform adapters.

#[test]
fn test_gateway_config_default() {
    let config = hermes_gateway::config::GatewayConfig::default();
    assert!(config.platforms.is_empty());
}

#[test]
fn test_parse_platform_variants() {
    use hermes_gateway::config::parse_platform;

    assert_eq!(parse_platform("telegram").unwrap(), hermes_gateway::config::Platform::Telegram);
    assert_eq!(parse_platform("feishu").unwrap(), hermes_gateway::config::Platform::Feishu);
    assert_eq!(parse_platform("wecom").unwrap(), hermes_gateway::config::Platform::Wecom);
}
