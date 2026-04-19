#![allow(dead_code)]
//! Interactive setup wizard for Hermes Agent.
//!
//! Mirrors the Python `hermes_cli/setup.py`.
//! Modular wizard with independently-runnable sections.

use console::{Style, Term};
use std::io;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Style presets
// ---------------------------------------------------------------------------

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }
fn bold() -> Style { Style::new().bold() }
fn red() -> Style { Style::new().red() }

// ---------------------------------------------------------------------------
// Home & config paths
// ---------------------------------------------------------------------------

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn config_path() -> PathBuf {
    get_hermes_home().join("config.yaml")
}

fn env_path() -> PathBuf {
    get_hermes_home().join(".env")
}

fn load_config() -> serde_yaml::Value {
    let path = config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_yaml::from_str(&content) {
                return value;
            }
        }
    }
    serde_yaml::Value::Mapping(Default::default())
}

fn save_config(config: &serde_yaml::Value) -> io::Result<()> {
    let path = config_path();
    if let Some(home) = path.parent() {
        std::fs::create_dir_all(home)?;
    }

    let yaml = serde_yaml::to_string(config)
        .map_err(io::Error::other)?;
    std::fs::write(&path, yaml)
}

fn append_env(key: &str, value: &str) -> io::Result<()> {
    let path = env_path();
    if let Some(home) = path.parent() {
        std::fs::create_dir_all(home)?;
    }

    let mut content = String::new();
    if path.exists() {
        content = std::fs::read_to_string(&path)?;
    }

    // Remove existing key if present
    let lines: Vec<&str> = content.lines().collect();
    let filtered: Vec<&str> = lines
        .into_iter()
        .filter(|line| !line.starts_with(&format!("{key}=")))
        .collect();
    content = filtered.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("{key}={value}\n"));

    std::fs::write(&path, content)
}

// ---------------------------------------------------------------------------
// Interactive input helpers
// ---------------------------------------------------------------------------

fn read_line(prompt: &str, default: Option<&str>) -> io::Result<String> {
    let term = Term::stdout();
    if let Some(d) = default {
        term.write_str(&format!("{} [{}]: ", prompt, dim().apply_to(d)))?;
    } else {
        term.write_str(&format!("{}: ", prompt))?;
    }
    term.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_string();

    if input.is_empty() {
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
    Ok(input)
}

fn read_secret(prompt: &str) -> io::Result<String> {
    let term = Term::stdout();
    term.write_str(&format!("{}: ", prompt))?;
    term.flush()?;
    let input = term.read_line()?;
    Ok(input.trim().to_string())
}

fn select_option(prompt: &str, options: &[&str], default: usize) -> io::Result<usize> {
    let term = Term::stdout();
    term.write_str(&format!("\n{}\n", bold().apply_to(prompt)))?;
    for (i, opt) in options.iter().enumerate() {
        if i == default {
            term.write_str(&format!("  {} {} (default)\n", green().apply_to("→"), bold().apply_to(opt)))?;
        } else {
            term.write_str(&format!("  {} {}\n", dim().apply_to(" "), opt))?;
        }
    }
    term.write_str(&format!("\nSelect [{}]: ", default))?;
    term.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_string();

    if input.is_empty() {
        return Ok(default);
    }
    if let Ok(idx) = input.parse::<usize>() {
        if idx < options.len() {
            return Ok(idx);
        }
    }
    Ok(default)
}

fn confirm(prompt: &str) -> io::Result<bool> {
    let term = Term::stdout();
    term.write_str(&format!("{} [Y/n]: ", prompt))?;
    term.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    Ok(input.is_empty() || input == "y" || input == "yes")
}

// ---------------------------------------------------------------------------
// Wizard sections
// ---------------------------------------------------------------------------

/// Section 1: Model & Provider setup.
fn setup_model_provider(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Model & Provider ━━━"));
    println!("\n{}", dim().apply_to("Choose your AI provider and enter API credentials."));
    println!("Supported: OpenRouter, OpenAI, Anthropic, DeepSeek, Google, Custom\n");

    let providers = [
        "openrouter",
        "openai",
        "anthropic",
        "deepseek",
        "google",
        "custom",
    ];

    // Determine current provider
    let current_provider = config
        .get("model")
        .and_then(|m| m.get("provider"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    let default_idx = providers.iter().position(|&p| p == current_provider).unwrap_or(0);
    let idx = select_option("Select provider:", &providers, default_idx)?;
    let provider = providers[idx];

    // Set provider in config
    set_config_value(config, &["model", "provider"], provider.to_string());

    // Get API key
    let env_key = match provider {
        "openrouter" => "OPENROUTER_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "google" => "GOOGLE_API_KEY",
        "custom" => {
            // Custom provider: ask for base_url + api_key
            let base_url = read_line("API base URL", None)?;
            set_config_value(config, &["model", "base_url"], base_url);

            let api_key = read_secret("API key")?;
            append_env("CUSTOM_API_KEY", &api_key)?;
            set_config_value(config, &["model", "api_mode"], "openai".to_string());

            println!("\n  {} Custom provider configured.", green().apply_to("✓"));
            return Ok(());
        }
        _ => "API_KEY",
    };

    let api_key = read_secret(env_key)?;
    if !api_key.is_empty() {
        append_env(env_key, &api_key)?;
        println!("  {} API key saved to .env", green().apply_to("✓"));
    }

    // Model selection
    let default_models = match provider {
        "openrouter" => vec![
            "anthropic/claude-opus-4-6",
            "anthropic/claude-sonnet-4-6",
            "openai/gpt-4o",
            "google/gemini-2.5-pro",
        ],
        "openai" => vec![
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-5",
            "gpt-5-mini",
        ],
        "anthropic" => vec![
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ],
        "deepseek" => vec![
            "deepseek-chat",
            "deepseek-reasoner",
        ],
        "google" => vec![
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ],
        _ => vec!["custom-model"],
    };

    println!("\nRecommended models for {}:", cyan().apply_to(provider));
    for (i, m) in default_models.iter().enumerate() {
        println!("  {}. {}", i + 1, m);
    }

    let model = read_line("\nModel name", Some(default_models[0]))?;
    set_config_value(config, &["model", "name"], model);

    // Base URL for non-standard providers
    if provider == "openrouter" {
        let has_base_url = config
            .get("model")
            .and_then(|m| m.get("base_url"))
            .is_some();
        if !has_base_url
            && confirm("Use default OpenRouter endpoint?")? {
                set_config_value(
                    config,
                    &["model", "base_url"],
                    "https://openrouter.ai/api/v1".to_string(),
                );
            }
    }

    println!("\n  {} Provider: {}", green().apply_to("✓"), provider);
    if let Some(name) = config.get("model").and_then(|m| m.get("name")).and_then(|n| n.as_str()) {
        println!("  Model: {}", name);
    }

    Ok(())
}

/// Section 2: Terminal Backend setup.
fn setup_terminal_backend(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Terminal Backend ━━━"));
    println!("\n{}", dim().apply_to("Where should commands be executed?"));
    println!("This determines the environment for running shell commands.\n");

    let backends = [
        ("local", "Local machine (fastest, no setup required)"),
        ("docker", "Docker container (isolated, reproducible)"),
        ("ssh", "Remote SSH server"),
        ("modal", "Modal cloud sandbox (serverless)"),
        ("singularity", "Singularity/Apptainer container"),
        ("daytona", "Daytona remote environment"),
    ];

    let current_backend = config
        .get("terminal")
        .and_then(|t| t.get("backend"))
        .and_then(|b| b.as_str())
        .unwrap_or("local");
    let default_idx = backends.iter().position(|(b, _)| *b == current_backend).unwrap_or(0);

    let options: Vec<String> = backends
        .iter()
        .map(|(name, desc)| format!("{name} — {desc}"))
        .collect();
    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();

    let idx = select_option("Select terminal backend:", &option_refs, default_idx)?;
    let (backend, _) = backends[idx];

    set_config_value(config, &["terminal", "backend"], backend.to_string());

    // Backend-specific config
    match backend {
        "docker" => {
            let image = read_line("Docker image", Some("ubuntu:22.04"))?;
            set_config_value(config, &["terminal", "docker_image"], image);
        }
        "ssh" => {
            let host = read_line("SSH host (e.g., user@hostname)", None)?;
            set_config_value(config, &["terminal", "ssh_host"], host);
            let user = read_line("SSH user", None)?;
            set_config_value(config, &["terminal", "ssh_user"], user);
        }
        "modal" => {
            println!("\n  {} Modal requires a Modal account and auth token.", yellow().apply_to("Note"));
            let token = read_secret("Modal auth token", ).ok();
            if let Some(t) = token {
                append_env("MODAL_AUTH_TOKEN", &t)?;
            }
        }
        _ => {}
    }

    println!("\n  {} Backend: {}", green().apply_to("✓"), backend);
    Ok(())
}

/// Section 3: Agent Settings.
fn setup_agent_settings(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Agent Settings ━━━"));
    println!("\n{}", dim().apply_to("Configure behavior, context compression, and session settings."));

    // Max iterations
    let current_iter = config
        .get("agent")
        .and_then(|a| a.get("max_iterations"))
        .and_then(|n| n.as_u64())
        .unwrap_or(90);
    let iter_str = read_line("Max iterations per turn", Some(&current_iter.to_string()))?;
    if let Ok(n) = iter_str.parse::<u64>() {
        set_config_value(config, &["agent", "max_iterations"], n.to_string());
    }

    // Context compression
    if confirm("Enable context compression? (recommended for long sessions)")? {
        set_config_value(config, &["compression", "enabled"], "true".to_string());

        let target = read_line("Target token count after compression", Some("4000"))?;
        if let Ok(n) = target.parse::<u64>() {
            set_config_value(config, &["compression", "target_tokens"], n.to_string());
        }

        let protect = read_line("Protect first N lines from compression", Some("30"))?;
        if let Ok(n) = protect.parse::<usize>() {
            set_config_value(config, &["compression", "protect_first_n"], n.to_string());
        }

        // Summary model
        if confirm("Use a different model for summarization?")? {
            let summary_model = read_line("Summary model", Some("anthropic/claude-haiku-4-5-20251001"))?;
            set_config_value(config, &["compression", "model"], summary_model);
        }
    } else {
        set_config_value(config, &["compression", "enabled"], "false".to_string());
    }

    // Memory review interval
    let memory_interval = read_line("Memory review interval (turns)", Some("10"))?;
    if let Ok(n) = memory_interval.parse::<usize>() {
        set_config_value(config, &["memory", "nudge_interval"], n.to_string());
    }

    println!("\n  {} Agent settings configured.", green().apply_to("✓"));
    Ok(())
}

/// Section 4: Gateway / Messaging Platforms.
fn setup_gateway(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Messaging Gateway ━━━"));
    println!("\n{}", dim().apply_to("Connect Hermes to messaging platforms."));
    println!("Available: Telegram, Discord, Slack, WhatsApp, Signal, Feishu, Webhook\n");

    let platforms = [
        "telegram",
        "discord",
        "slack",
        "whatsapp",
        "signal",
        "feishu",
        "webhook",
    ];

    let mut selected: Vec<String> = Vec::new();

    for platform in &platforms {
        if confirm(&format!("Enable {platform}?"))? {
            let token = read_secret(&format!("{platform} token/bot key"))?;
            if !token.is_empty() {
                let env_key = match *platform {
                    "telegram" => "TELEGRAM_BOT_TOKEN",
                    "discord" => "DISCORD_BOT_TOKEN",
                    "slack" => "SLACK_BOT_TOKEN",
                    "whatsapp" => "WHATSAPP_TOKEN",
                    "signal" => "SIGNAL_TOKEN",
                    "feishu" => "FEISHU_APP_SECRET",
                    "webhook" => "WEBHOOK_SECRET",
                    _ => "PLATFORM_TOKEN",
                };
                append_env(env_key, &token)?;
                selected.push(platform.to_string());
            }
        }
    }

    if !selected.is_empty() {
        // Set gateway enabled
        set_config_value(config, &["gateway", "enabled"], "true".to_string());

        // Platform list
        let platforms_yaml = serde_yaml::Value::Sequence(
            selected.iter().map(|s| serde_yaml::Value::String(s.clone())).collect(),
        );
        if let Some(map) = config.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("gateway".to_string()),
                serde_yaml::Value::Mapping({
                    let mut m = serde_yaml::Mapping::new();
                    m.insert(
                        serde_yaml::Value::String("enabled".to_string()),
                        serde_yaml::Value::Bool(true),
                    );
                    m.insert(
                        serde_yaml::Value::String("platforms".to_string()),
                        platforms_yaml,
                    );
                    m
                }),
            );
        }

        println!("\n  {} Platforms configured: {}", green().apply_to("✓"), selected.join(", "));
    } else {
        println!("\n  {} No platforms configured.", dim().apply_to("○"));
        println!("  You can run `hermes setup gateway` later to add platforms.");
    }

    Ok(())
}

/// Section 5: Tools configuration.
fn setup_tools(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Tool Configuration ━━━"));
    println!("\n{}", dim().apply_to("Configure optional tool integrations."));

    // TTS
    if confirm("Configure Text-to-Speech (TTS)?")? {
        setup_tts(config)?;
    }

    // Web search
    if confirm("Configure web search (Firecrawl)?")? {
        let api_key = read_secret("FIRECRAWL_API_KEY")?;
        if !api_key.is_empty() {
            append_env("FIRECRAWL_API_KEY", &api_key)?;
            println!("  {} Firecrawl API key saved.", green().apply_to("✓"));
        }
    }

    if confirm("Configure web search (Exa)?")? {
        let api_key = read_secret("EXA_API_KEY")?;
        if !api_key.is_empty() {
            append_env("EXA_API_KEY", &api_key)?;
            println!("  {} Exa API key saved.", green().apply_to("✓"));
        }
    }

    // Image generation
    if confirm("Configure image generation?")? {
        let provider = read_line("Image provider (openai, stability)", Some("openai"))?;
        set_config_value(config, &["image_gen", "provider"], provider);

        // Image gen uses the main API key, no extra config needed
        println!("  {} Image generation configured (uses primary API key).", green().apply_to("✓"));
    }

    // Voice transcription
    if confirm("Configure voice transcription (OpenAI Whisper)?")? {
        // Uses the primary OpenAI API key
        println!("  {} Voice transcription configured (uses OpenAI API key).", green().apply_to("✓"));
    }

    Ok(())
}

/// TTS sub-section.
fn setup_tts(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", cyan().apply_to("  ── TTS ──"));

    let providers = ["openai", "elevenlabs", "edge-tts", "neutts"];
    let default_idx = 0;

    let idx = select_option("TTS provider:", &providers, default_idx)?;
    let provider = providers[idx];

    set_config_value(config, &["tts", "provider"], provider.to_string());

    match provider {
        "elevenlabs" => {
            let api_key = read_secret("ELEVENLABS_API_KEY")?;
            if !api_key.is_empty() {
                append_env("ELEVENLABS_API_KEY", &api_key)?;
            }
        }
        "openai" => {
            println!("  {} Uses OpenAI API key from provider setup.", dim().apply_to("○"));
        }
        "edge-tts" => {
            println!("  {} No API key required (Microsoft Edge TTS).", dim().apply_to("○"));
        }
        "neutts" => {
            println!("  {} Local NeuTTS engine (no API key needed).", dim().apply_to("○"));
        }
        _ => {}
    }

    let voice = read_line("Voice name (default varies by provider)", None);
    if let Ok(v) = voice {
        if !v.is_empty() {
            set_config_value(config, &["tts", "voice"], v);
        }
    }

    println!("  {} TTS configured: {}", green().apply_to("✓"), provider);
    Ok(())
}

// ---------------------------------------------------------------------------
// Config value setter
// ---------------------------------------------------------------------------

/// Set a nested config value. Supports dotted paths like ["compression", "enabled"].
fn set_config_value(config: &mut serde_yaml::Value, path: &[&str], value: String) {
    if path.is_empty() {
        return;
    }
    if let Some(map) = config.as_mapping_mut() {
        let mut current = map;
        for &key in &path[..path.len() - 1] {
            let key_val = serde_yaml::Value::String(key.to_string());
            let entry = current
                .entry(key_val)
                .or_insert(serde_yaml::Value::Mapping(Default::default()));
            if entry.is_mapping() {
                current = entry.as_mapping_mut().expect("entry should be a mapping");
            } else {
                // Replace with mapping
                *entry = serde_yaml::Value::Mapping(Default::default());
                current = entry.as_mapping_mut().expect("entry should be a mapping after replacement");
            }
        }
        let last_key = serde_yaml::Value::String(path.last().unwrap().to_string());
        // Try to parse value as appropriate type
        let yaml_value = if value == "true" {
            serde_yaml::Value::Bool(true)
        } else if value == "false" {
            serde_yaml::Value::Bool(false)
        } else if let Ok(n) = value.parse::<i64>() {
            serde_yaml::Value::Number(n.into())
        } else {
            serde_yaml::Value::String(value)
        };
        current.insert(last_key, yaml_value);
    }
}

// ---------------------------------------------------------------------------
// Quick setup mode — detect what's missing and only ask for essentials
// ---------------------------------------------------------------------------

fn run_quick_setup(config: &mut serde_yaml::Value) -> io::Result<()> {
    println!("\n{}", bold().apply_to("━━━ Quick Setup ━━━"));
    println!("\n{}", dim().apply_to("Configuring essential settings only."));

    let has_provider = config
        .get("model")
        .and_then(|m| m.get("provider"))
        .is_some();
    let has_api_key = std::env::var("OPENROUTER_API_KEY").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("DEEPSEEK_API_KEY").is_ok();

    if !has_provider || !has_api_key {
        println!("\n{}: No provider configured.", yellow().apply_to("Missing"));
        setup_model_provider(config)?;
    } else {
        println!("\n  {} Provider already configured.", green().apply_to("✓"));
    }

    println!("\n  {} Quick setup complete. Run `hermes setup` for full configuration.", green().apply_to("✓"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the full setup wizard.
pub fn cmd_setup() -> anyhow::Result<()> {
    let home = get_hermes_home();
    let is_first_install = !config_path().exists();

    println!();
    println!("{}", bold().apply_to("╔══════════════════════════════════════╗"));
    println!("{}", bold().apply_to("║     Hermes Agent Setup Wizard       ║"));
    println!("{}", bold().apply_to("╚══════════════════════════════════════╝"));
    println!("\nHERMES_HOME: {}", cyan().apply_to(home.display()));

    if is_first_install {
        println!("\n  First-time installation. Creating {}.", home.display());
        std::fs::create_dir_all(&home)?;
    }

    let sections = [
        ("model", "Model & Provider"),
        ("terminal", "Terminal Backend"),
        ("agent", "Agent Settings"),
        ("gateway", "Messaging Gateway"),
        ("tools", "Tool Configuration"),
    ];

    if is_first_install {
        // First install: offer quick vs full
        println!("\nHow would you like to set up?");
        let options = [
            "Quick setup — just provider + model",
            "Full setup — configure everything",
        ];
        let idx = select_option("Setup mode:", &options, 0)?;

        let mut config = load_config();

        if idx == 0 {
            run_quick_setup(&mut config)?;
            save_config(&config)?;
            println!("\n  {} Done! Run `hermes` to start chatting.", green().apply_to("✓"));
            return Ok(());
        }
    }

    let mut config = load_config();

    println!("\nSelect sections to configure (comma-separated, or 'all'):");
    for (i, (key, desc)) in sections.iter().enumerate() {
        let status = match *key {
            "model" => {
                let has = config.get("model").and_then(|m| m.get("provider")).is_some();
                if has { "configured" } else { "missing" }
            }
            "terminal" => "local (default)" ,
            _ => "",
        };
        if !status.is_empty() {
            println!("  {}. {} — {} ({})", i + 1, desc, dim().apply_to("status"), status);
        } else {
            println!("  {}. {}", i + 1, desc);
        }
    }

    let selection = read_line("\nSections", Some("all"))?;
    let indices: Vec<usize> = if selection.to_lowercase() == "all" {
        (0..sections.len()).collect()
    } else {
        selection
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .map(|n| n - 1)
            .filter(|n| *n < sections.len())
            .collect()
    };

    if indices.is_empty() {
        println!("  {} No sections selected.", dim().apply_to("○"));
        return Ok(());
    }

    for idx in indices {
        let (key, _) = sections[idx];
        match key {
            "model" => setup_model_provider(&mut config)?,
            "terminal" => setup_terminal_backend(&mut config)?,
            "agent" => setup_agent_settings(&mut config)?,
            "gateway" => setup_gateway(&mut config)?,
            "tools" => setup_tools(&mut config)?,
            _ => {}
        }
    }

    save_config(&config)?;

    println!("\n{}", bold().apply_to("━━━ Setup Complete ━━━"));
    println!("\nConfig saved to: {}", cyan().apply_to(config_path().display()));
    println!("Env keys saved to: {}", cyan().apply_to(env_path().display()));
    println!("\nStart chatting with: {}", green().apply_to("hermes"));

    Ok(())
}

/// Reset configuration to defaults.
pub fn cmd_setup_reset() -> anyhow::Result<()> {
    let green = console::Style::new().green();
    let yellow = console::Style::new().yellow();

    for file in ["config.yaml", ".env", "SOUL.md"] {
        let path = get_hermes_home().join(file);
        if path.exists() {
            std::fs::remove_file(&path)?;
            println!("  {} Removed {file}", green.apply_to("✓"));
        } else {
            println!("  {} {file} not found", yellow.apply_to("→"));
        }
    }
    println!();
    Ok(())
}

/// Run setup for a specific section.
pub fn cmd_setup_section(section: &str, _non_interactive: bool) -> anyhow::Result<()> {
    let home = get_hermes_home();
    std::fs::create_dir_all(&home)?;

    let mut config = load_config();

    match section {
        "model" => setup_model_provider(&mut config)?,
        "terminal" => setup_terminal_backend(&mut config)?,
        "agent" => setup_agent_settings(&mut config)?,
        "gateway" => setup_gateway(&mut config)?,
        "tools" => setup_tools(&mut config)?,
        "tts" => setup_tts(&mut config)?,
        _ => {
            println!("  {} Unknown section: {}", red().apply_to("✗"), section);
            println!("  Available sections: model, terminal, agent, gateway, tools, tts");
            return Ok(());
        }
    }

    save_config(&config)?;
    println!("\n  {} Section '{section}' configured.", green().apply_to("✓"));
    Ok(())
}
