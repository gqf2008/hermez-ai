//! Hermes CLI Application — main app struct.
//!
//! Interactive CLI with reedline for input, console for output.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hermes_agent_engine::agent::{AIAgent, AgentConfig};
use hermes_agent_engine::agent::types::Message;
use hermes_core::{HermesConfig, Result};
use hermes_tools::registry::ToolRegistry;
use hermes_tools::register_all_tools;

/// Custom reedline prompt that uses the active skin's branding.
struct SkinPrompt {
    model: String,
}

impl SkinPrompt {
    fn new(model: String) -> Self {
        Self { model }
    }
}

impl reedline::Prompt for SkinPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let skin = crate::skin_engine::get_active_skin();
        let symbol = skin.get_branding("prompt_symbol", "❯ ");
        Cow::Owned(symbol)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _edit_mode: reedline::PromptEditMode) -> Cow<'_, str> {
        let skin = crate::skin_engine::get_active_skin();
        let symbol = skin.get_branding("prompt_symbol", "❯ ");
        Cow::Owned(symbol)
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: reedline::PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            reedline::PromptHistorySearchStatus::Passing => "",
            reedline::PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({}reverse-search: {}) ", prefix, history_search.term))
    }
}

/// Main application struct holding configuration and state.
pub struct HermesApp {
    #[allow(dead_code)]
    config: HermesConfig,
}

impl HermesApp {
    pub fn new() -> Result<Self> {
        let config = HermesConfig::load()?;
        Ok(Self { config })
    }

    /// Run the interactive chat loop.
    pub fn run_chat(
        &self,
        model: Option<String>,
        query: Option<String>,
        _image: Option<String>,
        _toolsets: Option<String>,
        _skills: Option<String>,
        _provider: Option<String>,
        _resume: Option<String>,
        _continue_last: Option<Option<String>>,
        _worktree: bool,
        _checkpoints: bool,
        max_turns: Option<u32>,
        _yolo: bool,
        _pass_session_id: bool,
        _source: Option<String>,
        quiet: bool,
        _verbose: bool,
        skip_context: bool,
        _skip_memory: bool,
        _voice: bool,
    ) -> Result<()> {
        let model_name = model
            .or_else(|| self.config.model.name.clone())
            .unwrap_or_else(|| "anthropic/claude-opus-4.6".to_string());

        // Build tool registry
        let mut registry = ToolRegistry::new();
        register_all_tools(&mut registry);

        if !quiet {
            println!("Hermes Agent — {}", model_name);
            println!("Tools: {} registered", registry.len());
            println!("Type /help for available commands, /quit to exit.");
            println!();
        }

        // Conversation history across turns
        let mut messages: Vec<Message> = Vec::new();
        let mut last_query: Option<String> = None;
        let mut session_title: Option<String> = None;
        let mut yolo_mode = false;

        // Resolve provider for default model fallback.
        // When no model is explicitly configured, fall back to the provider's
        // first catalog model so the API call doesn't fail with "model must be non-empty".
        let provider_str = model_name.split('/').next().unwrap_or("").to_lowercase();
        let provider = hermes_llm::provider::parse_provider(&provider_str);
        let final_model = if model_name.is_empty() {
            if let Some(default) = hermes_llm::provider::get_default_model_for_provider(provider.clone()) {
                tracing::info!("No model configured — defaulting to {default} for provider {}", provider);
                default.to_string()
            } else {
                "anthropic/claude-opus-4.6".to_string()
            }
        } else {
            model_name
        };

        // Build model config hashmap for runtime provider resolution
        let mut model_cfg = HashMap::new();
        if let Some(ref name) = self.config.model.name {
            model_cfg.insert("default".to_string(), serde_json::json!(name));
        }
        if let Some(ref provider) = self.config.model.provider {
            model_cfg.insert("provider".to_string(), serde_json::json!(provider));
        }
        if let Some(ref base_url) = self.config.model.base_url {
            model_cfg.insert("base_url".to_string(), serde_json::json!(base_url));
        }
        if let Some(ref api_key) = self.config.model.api_key {
            model_cfg.insert("api_key".to_string(), serde_json::json!(api_key));
        }
        if let Some(ref api_mode) = self.config.model.api_mode {
            model_cfg.insert("api_mode".to_string(), serde_json::json!(api_mode));
        }

        // Resolve runtime provider (credential pool → auth.json → env → config)
        let runtime = hermes_llm::runtime_provider::resolve_runtime_provider(
            self.config.model.provider.as_deref(),
            self.config.model.api_key.as_deref(),
            self.config.model.base_url.as_deref(),
            Some(&model_cfg),
        );

        let (resolved_model, resolved_base_url, resolved_api_key, resolved_provider, resolved_api_mode) =
            if let Some(ref rt) = runtime {
                let m = rt.model.clone().unwrap_or_else(|| final_model.clone());
                (
                    m,
                    Some(rt.base_url.clone()).filter(|s| !s.is_empty()),
                    Some(rt.api_key.clone()).filter(|s| !s.is_empty()),
                    Some(rt.provider.clone()).filter(|s| !s.is_empty()),
                    Some(rt.api_mode.clone()).filter(|s| !s.is_empty()),
                )
            } else {
                (final_model, self.config.model.base_url.clone(), self.config.model.api_key.clone(), self.config.model.provider.clone(), self.config.model.api_mode.clone())
            };

        // Build agent config
        let max_iterations = max_turns.unwrap_or(90) as usize;
        // Bridge config provider preferences to AgentConfig
        let provider_preferences = self.config.provider.as_ref().map(|p| {
            hermes_llm::client::ProviderPreferences {
                only: p.allowed.clone(),
                ignore: p.ignored.clone(),
                order: p.order.clone(),
                sort: p.sort.clone(),
                require_parameters: p.require_parameters,
                data_collection: p.data_collection.clone(),
            }
        });

        // Build credential pool from config if a strategy exists for the resolved provider
        let credential_pool = resolved_provider.as_deref().and_then(|provider| {
            self.config.credential_pool_strategies.get(provider).and_then(|strategy| {
                let mut pool = hermes_llm::credential_pool::from_entries(provider, strategy.credentials.clone())?;
                if let Some(mode) = strategy.mode.as_deref() {
                    let strategy_enum = match mode {
                        "round_robin" => hermes_llm::credential_pool::PoolStrategy::RoundRobin,
                        "failover" | "fill_first" => hermes_llm::credential_pool::PoolStrategy::FillFirst,
                        "random" => hermes_llm::credential_pool::PoolStrategy::Random,
                        "least_used" => hermes_llm::credential_pool::PoolStrategy::LeastUsed,
                        _ => hermes_llm::credential_pool::PoolStrategy::RoundRobin,
                    };
                    pool.set_strategy(strategy_enum);
                }
                Some(Arc::new(pool))
            })
        });

        let mut config = AgentConfig {
            model: resolved_model.clone(),
            max_iterations,
            skip_context_files: skip_context,
            terminal_cwd: std::env::current_dir().ok(),
            base_url: resolved_base_url,
            api_key: resolved_api_key,
            provider: resolved_provider,
            api_mode: resolved_api_mode,
            provider_preferences,
            credential_pool,
            ..AgentConfig::default()
        };

        let interrupt = Arc::new(AtomicBool::new(false));

        // Set up the event runtime for async agent calls
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| hermes_core::HermesError::new(
                hermes_core::ErrorCategory::InternalError,
                format!("Failed to create tokio runtime: {e}"),
            ))?;

        let mut agent = AIAgent::new(config.clone(), Arc::new(registry.clone()))?;

        // Wire up callbacks for real-time output when not in quiet mode
        if !quiet {
            agent.set_stream_callback(|delta| {
                print!("{}", delta);
                let _ = std::io::stdout().flush();
            });
            agent.set_tool_gen_started_callback(|name| {
                println!("\n  → Tool: {}", name);
            });
            agent.set_status_callback(|_event, msg| {
                tracing::debug!("Agent status: {msg}");
            });
        }

        // Single-shot query mode (non-interactive)
        if let Some(ref q) = query {
            let spinner = if !quiet {
                let s = indicatif::ProgressBar::new_spinner();
                s.set_style(
                    indicatif::ProgressStyle::default_spinner()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
                        .template("{spinner} {msg}")
                        .unwrap(),
                );
                s.set_message("Thinking...");
                s.enable_steady_tick(std::time::Duration::from_millis(100));
                Some(s)
            } else {
                None
            };

            let turn_result = rt.block_on(async {
                agent.run_conversation(q, None, None).await
            });

            if let Some(s) = spinner {
                s.finish_and_clear();
            }

            if !quiet && !turn_result.response.is_empty() {
                println!("{}", turn_result.response);
            }

            return Ok(());
        }

        // Set up reedline for input with tab completion and skin-aware prompt
        let mut line_editor = reedline::Reedline::create()
            .with_completer(Box::new(crate::tui::completers::HermesCompleter::new()));
        let prompt = SkinPrompt::new(resolved_model.clone());

        // Main chat loop
        loop {
            // Check for interrupt
            if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                println!("\nConversation interrupted.");
                break;
            }

            // Read input
            let read_result = line_editor.read_line(&prompt);
            let input = match read_result {
                Ok(reedline::Signal::Success(buffer)) => buffer,
                Ok(reedline::Signal::CtrlD) => {
                    println!();
                    break;
                }
                Ok(reedline::Signal::CtrlC) => {
                    println!("^C");
                    continue;
                }
                Err(e) => {
                    tracing::error!("Input error: {e}");
                    break;
                }
            };

            let trimmed = input.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Handle slash commands
            let mut should_exit = false;
            let mut agent_turn_prompt: Option<String> = None;

            if trimmed.starts_with('/') {
                let without_slash = &trimmed[1..];
                let (cmd, args) = match without_slash.find(' ') {
                    Some(pos) => (&without_slash[..pos], &without_slash[pos + 1..]),
                    None => (without_slash, ""),
                };

                let mut ctx = crate::slash_commands::SlashContext {
                    agent: &mut agent,
                    messages: &mut messages,
                    config: &mut config,
                    registry: &mut registry,
                    quiet,
                    last_query: &mut last_query,
                    session_title: &mut session_title,
                    yolo_mode: &mut yolo_mode,
                    should_exit: &mut should_exit,
                };

                match crate::slash_commands::dispatch(cmd, args, &mut ctx) {
                    crate::slash_commands::SlashResult::Handled => {
                        if should_exit {
                            break;
                        }
                        continue;
                    }
                    crate::slash_commands::SlashResult::AgentTurn(prompt) => {
                        agent_turn_prompt = Some(prompt);
                    }
                    crate::slash_commands::SlashResult::Error(err) => {
                        eprintln!("Error: {}", err);
                        continue;
                    }
                }
            }

            // Also support legacy bare commands for backward compatibility
            match trimmed.to_lowercase().as_str() {
                "quit" | "exit" | ":q" => break,
                "clear" | ":clear" => {
                    messages.clear();
                    last_query = None;
                    agent.reset_session_state();
                    println!("Context cleared.");
                    continue;
                }
                _ => {}
            }

            let prompt = agent_turn_prompt.unwrap_or_else(|| trimmed.to_string());
            last_query = Some(prompt.clone());

            // Show spinner during processing
            let spinner = if !quiet {
                let s = indicatif::ProgressBar::new_spinner();
                s.set_style(
                    indicatif::ProgressStyle::default_spinner()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
                        .template("{spinner} {msg}")
                        .unwrap(),
                );
                s.set_message("Thinking...");
                s.enable_steady_tick(std::time::Duration::from_millis(100));
                Some(s)
            } else {
                None
            };

            // Run the agent with conversation history
            let history_slice = if messages.is_empty() {
                None
            } else {
                Some(messages.as_slice())
            };
            let turn_result = rt.block_on(async {
                agent.run_conversation(&prompt, None, history_slice).await
            });

            // Update message history from turn result
            messages = turn_result.messages.clone();

            // Stop spinner
            if let Some(s) = spinner {
                s.finish_and_clear();
            }

            // Display result
            if !turn_result.response.is_empty() {
                println!("\n{}\n", turn_result.response);
            } else {
                // Show last assistant message from history
                for msg in turn_result.messages.iter().rev() {
                    if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
                        if role == "assistant" {
                            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                                if !content.is_empty() {
                                    println!("\n{}\n", content);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Show summary in non-quiet mode
            if !quiet {
                println!("[{} API calls, {} messages, {} budget remaining]",
                    turn_result.api_calls,
                    turn_result.messages.len(),
                    agent.budget.remaining(),
                );
                println!();
            }
        }

        if !quiet {
            println!("Goodbye.");
        }

        Ok(())
    }

    pub fn run_setup(&self) -> Result<()> {
        use console::Style;
        use dialoguer::{Confirm, Input};
        use std::fs;

        let green = Style::new().green();
        let yellow = Style::new().yellow();

        let home = hermes_core::get_hermes_home();
        println!("{} Hermes Setup", green.apply_to("Setup"));
        println!("  HERMES_HOME: {}", home.display());
        println!();

        // Ensure directories
        fs::create_dir_all(&home)?;
        fs::create_dir_all(home.join("skills"))?;
        fs::create_dir_all(home.join("bin"))?;
        println!("{} Directories created", green.apply_to("✓"));

        // Check .env file
        let env_path = home.join(".env");
        if env_path.exists() {
            println!("{} .env file exists at {}", green.apply_to("✓"), env_path.display());
        } else {
            println!("{} No .env file found", yellow.apply_to("→"));
            let create = Confirm::new()
                .with_prompt("Create .env file for API keys?")
                .default(true)
                .interact()
                .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
            if create {
                fs::write(&env_path, "# API Keys — uncomment and fill in:\n# OPENAI_API_KEY=\n# ANTHROPIC_API_KEY=\n# OPENROUTER_API_KEY=\n")?;
                println!("{} Created .env file at {}", green.apply_to("✓"), env_path.display());
            }
        }

        // Check config file
        let config_path = home.join("config.yaml");
        if config_path.exists() {
            println!("{} config.yaml exists at {}", green.apply_to("✓"), config_path.display());
        } else {
            println!("{} No config.yaml found", yellow.apply_to("→"));
            let create = Confirm::new()
                .with_prompt("Create default config.yaml?")
                .default(true)
                .interact()
                .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
            if create {
                let default_config = serde_yaml::to_string(&serde_yaml::Mapping::new())
                    .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
                fs::write(&config_path, default_config)?;
                println!("{} Created config.yaml at {}", green.apply_to("✓"), config_path.display());
            }
        }

        // Prompt for primary model
        println!();
        let model: String = Input::new()
            .with_prompt("Primary model (e.g., anthropic/claude-opus-4.6)")
            .default("anthropic/claude-opus-4.6".to_string())
            .interact_text()
            .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
        println!("{} Model set to: {}", green.apply_to("✓"), model);

        // Prompt for SOUL.md
        let soul_path = home.join("SOUL.md");
        if !soul_path.exists() {
            println!();
            let create_soul = Confirm::new()
                .with_prompt("Create SOUL.md (agent personality/instructions)?")
                .default(true)
                .interact()
                .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
            if create_soul {
                let prompt_text: String = Input::new()
                    .with_prompt("Agent personality (brief description)")
                    .default("You are a helpful AI assistant.".to_string())
                    .interact_text()
                    .map_err(|e| hermes_core::HermesError::from(std::io::Error::other(e.to_string())))?;
                fs::write(&soul_path, format!("# SOUL.md\n\n{prompt_text}\n"))?;
                println!("{} Created SOUL.md", green.apply_to("✓"));
            }
        }

        println!();
        println!("{} Setup complete!", green.apply_to("Done"));
        Ok(())
    }

    pub fn list_tools(&self) -> Result<()> {
        let mut registry = ToolRegistry::new();
        register_all_tools(&mut registry);

        let tools = registry.list_tools();
        println!("Registered tools: {}", tools.len());
        println!();

        let available = registry.get_available_tools();
        println!("Available tools (prerequisites met): {}", available.len());
        for entry in &available {
            println!("  {}  {}  {}", entry.emoji, entry.name, entry.description);
        }

        let toolsets = registry.list_toolsets();
        println!();
        println!("Toolsets: {:?}", toolsets);

        Ok(())
    }

    pub fn show_tool_info(&self, name: &str) -> Result<()> {
        let mut registry = ToolRegistry::new();
        register_all_tools(&mut registry);

        if let Some(entry) = registry.get(name) {
            println!("Tool: {}", entry.name);
            println!("Toolset: {}", entry.toolset);
            println!("Description: {}", entry.description);
            println!("Emoji: {}", entry.emoji);
            if !entry.requires_env.is_empty() {
                println!("Required env vars: {:?}", entry.requires_env);
            }
            println!();
            println!("Schema:");
            println!("{}", serde_json::to_string_pretty(&entry.schema)?);
        } else {
            println!("Tool '{}' not found.", name);
            let tools = registry.list_tools();
            println!("Available tools: {:?}", tools);
        }

        Ok(())
    }

    pub fn list_tools_for_platform(&self, platform: &str) -> Result<()> {
        use console::Style;
        let dim = Style::new().dim();

        let mut registry = ToolRegistry::new();
        register_all_tools(&mut registry);

        println!("Tools for platform: {}", platform);
        println!();

        let tools = registry.list_tools();
        let mut enabled_count = 0;
        let mut disabled_count = 0;

        // Get disabled tools from config
        let home = if let Ok(h) = std::env::var("HERMES_HOME") {
            std::path::PathBuf::from(h)
        } else if let Some(dir) = dirs::home_dir() {
            dir.join(".hermes")
        } else {
            std::path::PathBuf::from(".hermes")
        };
        let config_path = home.join("config.yaml");
        let disabled_tools: std::collections::HashSet<String> = if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                    config.get("tools")
                        .and_then(|t| t.get(platform))
                        .and_then(|p| p.as_sequence())
                        .map(|seq| seq.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| {
                                s.strip_prefix('!').unwrap_or(s).to_string()
                            })
                            .collect())
                        .unwrap_or_default()
                } else {
                    Default::default()
                }
            } else {
                Default::default()
            }
        } else {
            Default::default()
        };

        for tool_name in &tools {
            let is_disabled = disabled_tools.contains(tool_name)
                || disabled_tools.iter().any(|d| d.starts_with("mcp:"));
            if is_disabled {
                println!("  {} {}", dim.apply_to("○"), tool_name);
                disabled_count += 1;
            } else {
                println!("  ✓ {}", tool_name);
                enabled_count += 1;
            }
        }

        println!();
        println!("  {} enabled, {} disabled", enabled_count, disabled_count);

        Ok(())
    }

    pub fn list_skills(&self) -> Result<()> {
        use console::Style;
        let green = Style::new().green();
        let yellow = Style::new().yellow();
        let dim = Style::new().dim();

        let result = hermes_tools::skills::handle_skills_list(serde_json::json!({}));
        match result {
            Ok(json_str) => {
                let json: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| anyhow::anyhow!("Failed to parse skill data: {e}"))?;
                if json.get("error").is_some() {
                    println!("{} {}", yellow.apply_to("!"), json["error"]);
                    return Ok(());
                }

                let skills = json["skills"].as_array();
                let categories = json["categories"].as_array();
                let count = json["count"].as_u64().unwrap_or(0);

                println!("Installed skills: {}", count);
                if let Some(cats) = categories {
                    println!("Categories: {}", cats.iter().map(|v| v.as_str().unwrap_or("")).collect::<Vec<_>>().join(", "));
                }
                println!();

                if let Some(arr) = skills {
                    if arr.is_empty() {
                        println!("{} No skills found.", dim.apply_to("→"));
                        return Ok(());
                    }
                    for skill in arr {
                        let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let desc = skill.get("description").and_then(|v| v.as_str()).unwrap_or("");
                        let category = skill.get("category").and_then(|v| v.as_str()).unwrap_or("");
                        let enabled = if skill.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
                            green.apply_to("enabled").to_string()
                        } else {
                            dim.apply_to("disabled").to_string()
                        };
                        println!("  {}  {}  {}  [{}]", name, dim.apply_to(desc), dim.apply_to(category), enabled);
                    }
                }
            }
            Err(e) => {
                eprintln!("Error listing skills: {e}");
            }
        }
        Ok(())
    }

    pub fn show_skill_info(&self, name: &str) -> Result<()> {
        use console::Style;
        let yellow = Style::new().yellow();
        let dim = Style::new().dim();

        let result = hermes_tools::skills::handle_skill_view(serde_json::json!({
            "name": name,
        }));
        match result {
            Ok(json_str) => {
                let json: serde_json::Value = serde_json::from_str(&json_str)
                    .map_err(|e| anyhow::anyhow!("Failed to parse skill data: {e}"))?;
                if json.get("error").is_some() {
                    println!("{} {}", yellow.apply_to("!"), json["error"]);
                    if let Some(available) = json.get("available_skills") {
                        if let Some(arr) = available.as_array() {
                            println!();
                            println!("{} Available skills:", dim.apply_to("→"));
                            for s in arr {
                                if let Some(sname) = s.as_str() {
                                    println!("    {}", sname);
                                }
                            }
                        }
                    }
                    return Ok(());
                }

                let skill_name = json.get("name").and_then(|v| v.as_str()).unwrap_or(name);
                let desc = json.get("description").and_then(|v| v.as_str()).unwrap_or("");
                let category = json.get("category").and_then(|v| v.as_str()).unwrap_or("");
                let tags = json.get("tags").and_then(|v| v.as_array());
                let enabled = json.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

                println!("Skill: {}", skill_name);
                println!("Category: {}", category);
                println!("Description: {}", desc);
                println!("Enabled: {}", enabled);
                if let Some(tags) = tags {
                    println!("Tags: {}", tags.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "));
                }
                if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                    println!();
                    println!("--- SKILL.md content ---");
                    println!("{content}");
                    println!("--- end ---");
                }
            }
            Err(e) => {
                eprintln!("Error viewing skill: {e}");
            }
        }
        Ok(())
    }

    pub fn enable_skill(&self, name: &str, platform: Option<&str>) -> Result<()> {
        use console::Style;
        let green = Style::new().green();
        let yellow = Style::new().yellow();

        let mut config = HermesConfig::load().unwrap_or_default();

        if let Some(p) = platform {
            let list = config
                .skills
                .platform_disabled
                .entry(p.to_string())
                .or_default();
            if list.contains(&name.to_string()) {
                list.retain(|s| s != name);
                println!("  {} Skill '{name}' enabled for platform '{p}'", green.apply_to("✓"));
            } else {
                println!("  {} Skill '{name}' was already enabled for platform '{p}'", yellow.apply_to("→"));
            }
        } else if config.skills.disabled.contains(&name.to_string()) {
            config.skills.disabled.retain(|s| s != name);
            println!("  {} Skill '{name}' enabled", green.apply_to("✓"));
        } else {
            println!("  {} Skill '{name}' was already enabled", yellow.apply_to("→"));
        }

        config.save()?;
        Ok(())
    }

    pub fn disable_skill(&self, name: &str, platform: Option<&str>) -> Result<()> {
        use console::Style;
        let green = Style::new().green();
        let yellow = Style::new().yellow();

        let mut config = HermesConfig::load().unwrap_or_default();

        if let Some(p) = platform {
            let list = config
                .skills
                .platform_disabled
                .entry(p.to_string())
                .or_default();
            if !list.contains(&name.to_string()) {
                list.push(name.to_string());
                println!("  {} Skill '{name}' disabled for platform '{p}'", green.apply_to("✓"));
            } else {
                println!("  {} Skill '{name}' was already disabled for platform '{p}'", yellow.apply_to("→"));
            }
        } else if !config.skills.disabled.contains(&name.to_string()) {
            config.skills.disabled.push(name.to_string());
            println!("  {} Skill '{name}' disabled", green.apply_to("✓"));
        } else {
            println!("  {} Skill '{name}' was already disabled", yellow.apply_to("→"));
        }

        config.save()?;
        Ok(())
    }

    pub fn list_skill_commands(&self) -> Result<()> {
        use console::Style;
        let cyan = Style::new().cyan();
        let dim = Style::new().dim();

        let commands = hermes_tools::skills::scan_skill_commands();

        println!();
        println!("{}", cyan.apply_to("◆ Skill Commands"));
        println!();

        if commands.is_empty() {
            println!("  {}", dim.apply_to("No skill commands found."));
            println!("  Install skills with: hermes skills install <name>");
            println!();
            return Ok(());
        }

        for (cmd, info) in &commands {
            println!("  {cmd:<20} {}", dim.apply_to(&info.name));
            println!("  {:<20} {}", "", dim.apply_to(&info.description));
        }
        println!();
        println!("  Total: {} command(s)", commands.len());
        println!();

        Ok(())
    }

    pub fn run_gateway(&self) -> Result<()> {
        use console::Style;
        use hermes_gateway::runner::{GatewayRunner, load_gateway_config, GatewayConfig};
        use hermes_gateway::config::Platform;

        let green = Style::new().green();
        let cyan = Style::new().cyan();
        let dim = Style::new().dim();

        println!("{} Hermes Gateway", cyan.apply_to("Gateway"));
        println!();

        // Load config
        let gateway_config = load_gateway_config();
        let platform_count = gateway_config.platforms.iter().filter(|p| p.enabled).count();
        println!("  {} {} platform(s) configured", green.apply_to("✓"), platform_count);

        if platform_count == 0 {
            println!("  No platforms configured. Set FEISHU_APP_ID/SECRET or WEIXIN_SESSION_KEY,");
            println!("  or add platforms to ~/.hermes/config.yaml under gateway.platforms");
            return Ok(());
        }

        // Create and initialize runner
        let mut runner = GatewayRunner::new(GatewayConfig {
            platforms: gateway_config.platforms,
            default_model: gateway_config.default_model.clone(),
        });
        runner.initialize();

        let status = runner.status();
        println!("  Feishu: {}", if status.feishu_configured { green.apply_to("configured").to_string() } else { dim.apply_to("not configured").to_string() });
        println!("  Weixin: {}", if status.weixin_configured { green.apply_to("configured").to_string() } else { dim.apply_to("not configured").to_string() });
        println!();

        // Set up message handler that routes to the agent engine
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, format!("Failed to create tokio runtime: {e}")))?;

        // Build agent for gateway use
        let model_name = gateway_config.default_model.clone();
        let mut agent_registry = ToolRegistry::new();
        register_all_tools(&mut agent_registry);

        // Provider default model fallback for gateway
        let provider_str = model_name.split('/').next().unwrap_or("openrouter").to_lowercase();
        let provider = hermes_llm::provider::parse_provider(&provider_str);
        let final_model = if model_name.is_empty() {
            if let Some(default) = hermes_llm::provider::get_default_model_for_provider(provider.clone()) {
                tracing::info!("No model configured — defaulting to {default} for provider {}", provider);
                default.to_string()
            } else {
                "anthropic/claude-opus-4.6".to_string()
            }
        } else {
            model_name
        };

        let agent_config = AgentConfig {
            model: final_model.clone(),
            max_iterations: 90,
            skip_context_files: false,
            terminal_cwd: std::env::current_dir().ok(),
            ..AgentConfig::default()
        };

        let agent = AIAgent::new(agent_config, Arc::new(agent_registry))
            .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, format!("Failed to create agent: {e}")))?;

        tracing::info!("Gateway started with {} platform(s) using model: {}", platform_count, final_model);
        println!("  Gateway running (Ctrl+C to stop)");

        // Create agent-based message handler
        struct AgentHandler {
            agent: tokio::sync::Mutex<AIAgent>,
        }

        #[async_trait::async_trait]
        impl hermes_gateway::runner::MessageHandler for AgentHandler {
            async fn handle_message(
                &self,
                _platform: Platform,
                chat_id: &str,
                content: &str,
                model_override: Option<&str>,
            ) -> std::result::Result<hermes_gateway::runner::HandlerResult, String> {
                tracing::info!("Gateway received from {chat_id}: {}", content.chars().take(50).collect::<String>());

                let mut agent = self.agent.lock().await;
                if let Some(model) = model_override {
                    agent.switch_model(model, None, None, None);
                }
                let turn_result = agent.run_conversation(content, None, None).await;
                if turn_result.response.is_empty() {
                    Err("Agent returned no response".to_string())
                } else {
                    Ok(hermes_gateway::runner::HandlerResult {
                        response: turn_result.response.clone(),
                        messages: turn_result.messages.iter().map(|arc| (**arc).clone()).collect(),
                        compression_exhausted: turn_result.compression_exhausted,
                        usage: turn_result.usage.map(|u| hermes_gateway::runner::TokenUsage {
                            prompt_tokens: u.prompt_tokens,
                            completion_tokens: u.completion_tokens,
                            total_tokens: u.total_tokens,
                        }),
                    })
                }
            }

            fn interrupt(&self, _chat_id: &str, _new_message: &str) {
                // Signal the agent to stop the current turn immediately.
                // The new message will be queued and processed after this
                // turn completes. Mirrors Python PR a8b7db35.
                let agent = self.agent.try_lock();
                if let Ok(mut a) = agent {
                    a.close();
                } else {
                    tracing::debug!("Agent handler locked during interrupt — flag already set");
                }
            }
        }

        rt.block_on(async {
            let handler = std::sync::Arc::new(AgentHandler {
                agent: tokio::sync::Mutex::new(agent),
            });
            runner.set_message_handler(handler).await;
            runner.run().await
                .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, e))
        })?;

        Ok(())
    }

    pub fn run_gateway_with_opts(&self, _verbose: bool, _quiet: bool, _replace: bool) -> Result<()> {
        // Delegate to existing implementation; verbose/quiet/replace flags
        // are reserved for future gateway runner enhancements.
        self.run_gateway()
    }

    pub fn run_doctor(&self) -> Result<()> {
        let _ = &self.config; // suppress unused warning
        crate::doctor_cmd::cmd_doctor()
            .map_err(|e| hermes_core::HermesError::new(
                hermes_core::ErrorCategory::InternalError,
                format!("Doctor failed: {e}"),
            ))
    }

    /// Run doctor in auto-fix mode — attempt to resolve detected issues.
    pub fn run_doctor_fix(&self) -> Result<()> {
        let _ = &self.config; // suppress unused warning
        crate::doctor_cmd::cmd_doctor_fix()
            .map_err(|e| hermes_core::HermesError::new(
                hermes_core::ErrorCategory::InternalError,
                format!("Doctor fix failed: {e}"),
            ))
    }

    pub fn list_models(&self) -> Result<()> {
        use console::Style;

        let green = Style::new().green();
        let yellow = Style::new().yellow();
        let cyan = Style::new().cyan();
        let dim = Style::new().dim();

        println!();
        println!("{}", cyan.apply_to("◆ Available Providers"));
        println!();

        let providers = [
            ("openrouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", true),
            ("nous", "https://api.nousresearch.com/v1", "NOUS_API_KEY", false),
            ("anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY", false),
            ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY", false),
            ("gemini", "https://generativelanguage.googleapis.com/...", "GOOGLE_API_KEY", false),
            ("zai", "https://api.z.ai/api/paas/v4/", "ZAI_API_KEY", false),
            ("kimi", "https://api.moonshot.cn/v1", "KIMI_API_KEY", false),
            ("minimax", "https://api.minimax.io/v1", "MINIMAX_API_KEY", false),
            ("codex", "https://api.openai.com/v1", "OPENAI_API_KEY", false),
        ];

        println!("{:<14} {:<50} {:<22} {:<10}", "Provider", "Base URL", "Env Var", "Status");
        println!("{}", "-".repeat(100));

        for (name, url, env_var, is_aggregator) in &providers {
            let has_key = std::env::var(env_var).is_ok();
            let status = if has_key {
                green.apply_to("✓ configured").to_string()
            } else {
                yellow.apply_to("⚠ not set").to_string()
            };
            let label = if *is_aggregator {
                format!("{name} (agg)")
            } else {
                name.to_string()
            };
            println!("{:<14} {:<50} {:<22} {}", label, url, env_var, status);
        }

        println!();
        println!("  {}", dim.apply_to("Fallback chain: openrouter → nous → codex → gemini → zai → kimi → minimax → anthropic"));
        println!();

        // Current model
        let model = &self.config.model.name.as_deref().unwrap_or("anthropic/claude-opus-4.6");
        println!("  {} Current model: {}", green.apply_to("→"), model);

        // Custom base URL
        if let Some(base_url) = &self.config.model.base_url {
            println!("  {} Custom base URL: {}", green.apply_to("→"), base_url);
        }

        println!();

        Ok(())
    }

    /// List all profiles (delegates to profiles_cmd).
    pub fn list_profiles(&self) -> Result<()> {
        crate::profiles_cmd::cmd_profile_list()
            .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, e.to_string()))
    }

    /// Create a profile (delegates to profiles_cmd).
    pub fn create_profile(&self, name: &str) -> Result<()> {
        crate::profiles_cmd::cmd_profile_create(name, false, false, None, false)
            .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, e.to_string()))
    }

    /// Switch to a profile (delegates to profiles_cmd).
    pub fn use_profile(&self, name: &str) -> Result<()> {
        crate::profiles_cmd::cmd_profile_use(name)
            .map_err(|e| hermes_core::HermesError::new(hermes_core::ErrorCategory::InternalError, e.to_string()))
    }
}
