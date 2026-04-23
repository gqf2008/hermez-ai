//! Integration tests for hermez-llm provider resolution.

#[test]
fn test_provider_parse_variants() {
    use hermez_llm::provider::parse_provider;

    assert_eq!(parse_provider("anthropic"), hermez_llm::provider::ProviderType::Anthropic);
    assert_eq!(parse_provider("openai"), hermez_llm::provider::ProviderType::OpenAI);
    assert_eq!(parse_provider("openrouter"), hermez_llm::provider::ProviderType::OpenRouter);
    assert_eq!(parse_provider("gemini"), hermez_llm::provider::ProviderType::Gemini);
}

#[test]
fn test_model_normalize_roundtrip() {
    use hermez_llm::model_normalize::normalize_model_for_provider;

    let cases = vec![
        ("claude-opus-4", "anthropic", "claude-opus-4"),
        ("gpt-4o", "openai", "gpt-4o"),
    ];

    for (model, provider, expected) in cases {
        let normalized = normalize_model_for_provider(model, provider);
        assert_eq!(normalized, expected);
    }
}

#[test]
fn test_token_estimate_via_model_metadata() {
    use hermez_llm::model_metadata::estimate_tokens_rough;
    let text = "Hello world, this is a test message for token estimation.";
    let tokens = estimate_tokens_rough(text);
    // Rough check: ~4 chars per token
    assert!(tokens > 5 && tokens < 50);
}
