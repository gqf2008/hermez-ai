#![allow(dead_code)]
//! Webhook subscription management.

use console::Style;
use std::path::PathBuf;

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn green() -> Style { Style::new().green() }
fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }

/// Webhook subscription record.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct WebhookSubscription {
    name: String,
    prompt: String,
    events: Vec<String>,
    description: String,
    deliver: String,
    deliver_chat_id: Option<String>,
    skills: Vec<String>,
    created_at: String,
}

fn webhook_file() -> PathBuf {
    get_hermes_home().join("webhooks.json")
}

fn load_subscriptions() -> Vec<WebhookSubscription> {
    let path = webhook_file();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(subs) = serde_json::from_str::<Vec<WebhookSubscription>>(&content) {
                return subs;
            }
        }
    }
    Vec::new()
}

fn save_subscriptions(subs: &[WebhookSubscription]) -> anyhow::Result<()> {
    let path = webhook_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(subs)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Subscribe to a webhook.
pub fn cmd_webhook_subscribe(
    name: &str,
    prompt: &str,
    events: &str,
    description: &str,
    deliver: &str,
    deliver_chat_id: Option<String>,
    skills: &str,
    _secret: Option<&str>,
) -> anyhow::Result<()> {
    let mut subs = load_subscriptions();

    // Remove existing with same name
    subs.retain(|s| s.name != name);

    let events_list: Vec<String> = if events.is_empty() {
        vec!["*".to_string()]
    } else {
        events.split(',').map(|s| s.trim().to_string()).collect()
    };
    let skills_list: Vec<String> = if skills.is_empty() {
        Vec::new()
    } else {
        skills.split(',').map(|s| s.trim().to_string()).collect()
    };

    subs.push(WebhookSubscription {
        name: name.to_string(),
        prompt: prompt.to_string(),
        events: events_list,
        description: description.to_string(),
        deliver: deliver.to_string(),
        deliver_chat_id,
        skills: skills_list,
        created_at: chrono::Local::now().to_rfc3339(),
    });

    save_subscriptions(&subs)?;

    println!("  {} Webhook subscription created: {}", green().apply_to("✓"), name);
    println!("    URL: /webhooks/{}", name);
    println!("    Deliver: {}", deliver);
    if !description.is_empty() {
        println!("    Description: {}", description);
    }

    Ok(())
}

/// List webhook subscriptions.
pub fn cmd_webhook_list() -> anyhow::Result<()> {
    let subs = load_subscriptions();

    println!();
    println!("{}", cyan().apply_to("◆ Webhook Subscriptions"));
    println!();

    if subs.is_empty() {
        println!("  {}", dim().apply_to("No webhook subscriptions configured."));
        println!("  Create one with: hermes webhook subscribe <name> --prompt \"...\"");
    } else {
        for sub in &subs {
            println!("  {}", green().apply_to(&sub.name));
            println!("    URL: /webhooks/{}", sub.name);
            println!("    Events: {}", sub.events.join(", "));
            println!("    Deliver: {}", sub.deliver);
            if !sub.description.is_empty() {
                println!("    Description: {}", sub.description);
            }
            if !sub.skills.is_empty() {
                println!("    Skills: {}", sub.skills.join(", "));
            }
            println!();
        }
    }
    println!();

    Ok(())
}

/// Remove a webhook subscription.
pub fn cmd_webhook_remove(name: &str) -> anyhow::Result<()> {
    let mut subs = load_subscriptions();
    let before = subs.len();
    subs.retain(|s| s.name != name);

    if subs.len() == before {
        println!("  {} Webhook not found: {}", yellow().apply_to("✗"), name);
    } else {
        save_subscriptions(&subs)?;
        println!("  {} Removed webhook: {}", green().apply_to("✓"), name);
    }

    Ok(())
}

/// Test a webhook by sending a sample payload.
pub fn cmd_webhook_test(name: &str, payload: &str) -> anyhow::Result<()> {
    let subs = load_subscriptions();
    let sub = subs.iter().find(|s| s.name == name);

    match sub {
        Some(sub) => {
            println!();
            println!("{}", cyan().apply_to("◆ Webhook Test"));
            println!();
            println!("  Subscription: {}", sub.name);
            println!("  Prompt template: {}", sub.prompt);
            println!("  Events: {}", sub.events.join(", "));
            println!();
            println!("  Payload:");
            if payload.is_empty() {
                println!("    {{\"test\": true, \"timestamp\": \"{}\"}}", chrono::Local::now().to_rfc3339());
            } else {
                println!("    {}", payload);
            }
            println!();
            println!("  {}", dim().apply_to("Note: This shows the config. Actual delivery requires the gateway to be running."));
            println!();
        }
        None => {
            println!("  {} Webhook not found: {}", yellow().apply_to("✗"), name);
        }
    }

    Ok(())
}

/// Dispatch webhook subcommands.
pub fn cmd_webhook(
    action: &str,
    name: Option<&str>,
    prompt: &str,
    events: &str,
    description: &str,
    deliver: &str,
    deliver_chat_id: Option<String>,
    skills: &str,
    payload: &str,
) -> anyhow::Result<()> {
    match action {
        "subscribe" | "add" => {
            let name = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_webhook_subscribe(name, prompt, events, description, deliver, deliver_chat_id, skills, None)
        }
        "list" | "ls" => cmd_webhook_list(),
        "remove" | "rm" => {
            let name = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_webhook_remove(name)
        }
        "test" => {
            let name = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_webhook_test(name, payload)
        }
        _ => {
            anyhow::bail!("Unknown action: {}. Use subscribe, list, remove, or test.", action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_empty_subscriptions() {
        // Clean up any existing file first
        std::fs::remove_file(webhook_file()).ok();
        let subs = load_subscriptions();
        assert!(subs.is_empty());
    }

    #[serial_test::serial(webhook_file)]
    #[test]
    fn test_subscribe_and_list() {
        // Clean up any existing file first
        std::fs::remove_file(webhook_file()).ok();

        // Save a test subscription
        let subs = vec![WebhookSubscription {
            name: "test_hook_subscribe".to_string(),
            prompt: "Hello {{body}}".to_string(),
            events: vec!["push".to_string()],
            description: "Test webhook".to_string(),
            deliver: "log".to_string(),
            deliver_chat_id: None,
            skills: Vec::new(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }];
        save_subscriptions(&subs).unwrap();

        let loaded = load_subscriptions();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test_hook_subscribe");

        // Clean up
        std::fs::remove_file(webhook_file()).ok();
    }

    #[serial_test::serial(webhook_file)]
    #[test]
    fn test_remove_webhook() {
        // Clean up any existing file first
        std::fs::remove_file(webhook_file()).ok();

        let subs = vec![WebhookSubscription {
            name: "to_remove_hook".to_string(),
            prompt: "".to_string(),
            events: vec![],
            description: "".to_string(),
            deliver: "log".to_string(),
            deliver_chat_id: None,
            skills: Vec::new(),
            created_at: "".to_string(),
        }];
        save_subscriptions(&subs).unwrap();

        cmd_webhook_remove("to_remove_hook").unwrap();
        let loaded = load_subscriptions();
        assert!(loaded.is_empty());

        // Clean up
        std::fs::remove_file(webhook_file()).ok();
    }
}
