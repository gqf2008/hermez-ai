#![allow(dead_code)]
//! Model management TUI.
//!
//! Mirrors Python: hermes model (interactive TUI for selecting/viewing models)

use console::Style;

fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }

/// Common model aliases.
/// Maps short names to canonical model identifiers.
fn resolve_model_alias(input: &str) -> String {
    let normalized = input.to_lowercase().trim().to_string();
    let resolved = match normalized.as_str() {
        "opus" | "claude-opus" => "anthropic/claude-opus-4-6",
        "sonnet" | "claude-sonnet" => "anthropic/claude-sonnet-4-6",
        "haiku" | "claude-haiku" => "anthropic/claude-haiku-4-5",
        "gpt-4o" | "4o" => "openai/gpt-4o",
        "gpt-4o-mini" | "4o-mini" => "openai/gpt-4o-mini",
        "o1" => "openai/o1",
        "o3-mini" => "openai/o3-mini",
        "gemini" | "gemini-pro" => "google/gemini-2.5-pro",
        "deepseek" | "deepseek-chat" => "deepseek/deepseek-chat",
        "nous" | "hermes" => "nous/hermes-3",
        _ => return input.to_string(),
    };
    resolved.to_string()
}

/// Extract provider from a canonical model string.
fn provider_from_model(model: &str) -> Option<&str> {
    model.split('/').next()
}

/// Get the expected API key env var for a provider.
fn api_key_env_for_provider(provider: &str) -> &str {
    match provider.to_lowercase().as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" | "codex" => "OPENAI_API_KEY",
        "google" | "gemini" => "GOOGLE_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "nous" => "NOUS_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "groq" => "GROQ_API_KEY",
        "zai" => "ZAI_API_KEY",
        "kimi" => "KIMI_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        _ => "API_KEY",
    }
}

/// Interactive model selection and management.
pub fn cmd_model() -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Model Management"));
    println!();
    println!("  {}", dim().apply_to("Available providers and models:"));
    println!();

    // List known providers
    let providers = [
        ("OpenRouter", "openrouter/", "router.openai.com"),
        ("OpenAI", "openai/", "api.openai.com"),
        ("Anthropic", "anthropic/", "api.anthropic.com"),
        ("Google", "google/", "generativelanguage.googleapis.com"),
        ("DeepSeek", "deepseek/", "api.deepseek.com"),
        ("Nous", "nous/", "api.nousresearch.com"),
    ];

    println!("  {}", green().apply_to("Providers:"));
    println!();
    for (name, prefix, host) in &providers {
        println!("    {name:15} {prefix:20} {host}");
    }

    println!();
    println!("  {}", dim().apply_to("Configure with: hermes config set model.provider <provider>"));
    println!("  {}", dim().apply_to("Configure with: hermes config set model.name <model>"));
    println!();

    Ok(())
}

/// Show available models list.
pub fn cmd_model_list() -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Available Models"));
    println!();

    // Common models
    let models = [
        ("anthropic", "claude-sonnet-4-6", "Anthropic Claude Sonnet 4.6"),
        ("anthropic", "claude-opus-4-6", "Anthropic Claude Opus 4.6"),
        ("anthropic", "claude-haiku-4-5", "Anthropic Claude Haiku 4.5"),
        ("openai", "gpt-4o", "OpenAI GPT-4o"),
        ("openai", "gpt-4o-mini", "OpenAI GPT-4o Mini"),
        ("openai", "o1", "OpenAI o1"),
        ("openai", "o3-mini", "OpenAI o3 Mini"),
        ("google", "gemini-2.5-pro", "Google Gemini 2.5 Pro"),
        ("deepseek", "deepseek-chat", "DeepSeek Chat V3"),
        ("nous", "hermes-3", "Nous Hermes 3"),
    ];

    println!("  {:15} {:25} Display Name", "Provider", "Model ID");
    println!("  {}", "-".repeat(60));
    for (provider, model_id, name) in &models {
        println!("  {:15} {:25} {}", provider, model_id, name);
    }
    println!();

    Ok(())
}

/// Switch to a different model and persist to config.
pub fn cmd_model_switch(model: &str) -> anyhow::Result<()> {
    let resolved = resolve_model_alias(model);

    // Validate format: should have a provider prefix
    let provider = provider_from_model(&resolved)
        .ok_or_else(|| anyhow::anyhow!("Invalid model format: '{}' (expected provider/model-id)", resolved))?;

    // Load current config
    let mut config = hermes_core::HermesConfig::load().unwrap_or_default();

    // Update model name
    config.model.name = Some(resolved.clone());

    // Optionally update provider if it differs
    if config.model.provider.as_deref() != Some(provider) {
        config.model.provider = Some(provider.to_string());
    }

    // Save config
    config.save()?;

    let env_var = api_key_env_for_provider(provider);
    let has_key = std::env::var(env_var).is_ok();

    println!();
    println!("  {} Model switched to {}", green().apply_to("✓"), green().apply_to(&resolved));
    println!("  {} Provider: {}", dim().apply_to("→"), provider);
    if has_key {
        println!("  {} API key: {} ({})", green().apply_to("✓"), env_var, green().apply_to("set"));
    } else {
        println!("  {} API key: {} ({})", yellow().apply_to("⚠"), env_var, yellow().apply_to("not set"));
        println!("     Set it with: export {}=<your-key>", env_var);
    }
    println!();

    Ok(())
}

/// Show detailed information about a model.
pub fn cmd_model_info(model: &str) -> anyhow::Result<()> {
    let resolved = resolve_model_alias(model);

    let provider = provider_from_model(&resolved)
        .ok_or_else(|| anyhow::anyhow!("Invalid model format: '{}'", resolved))?;

    let env_var = api_key_env_for_provider(provider);
    let has_key = std::env::var(env_var).is_ok();

    // Look up context length from metadata
    let context_length = hermes_llm::model_metadata::get_model_context_length(
        &resolved,
        "",  // base_url
        None, // config override
        provider,
        None, // endpoint metadata
    );

    println!();
    println!("{}", cyan().apply_to("◆ Model Information"));
    println!();
    println!("  Model ID:     {}", resolved);
    println!("  Provider:     {}", provider);
    println!("  API Key:      {} ({})", env_var,
        if has_key { green().apply_to("set").to_string() } else { yellow().apply_to("not set").to_string() }
    );
    println!("  Context:      {} tokens", context_length);

    // Show alias info if the input was resolved
    if resolved != model {
        println!();
        println!("  {} Resolved alias: '{}' → '{}'", dim().apply_to("→"), model, resolved);
    }

    println!();
    Ok(())
}
