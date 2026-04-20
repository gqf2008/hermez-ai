#![allow(dead_code)]
//! Insights command — session analytics and usage statistics.

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

fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }

/// Show session analytics and usage insights.
pub fn cmd_insights(_days: usize, _source: Option<&str>) -> anyhow::Result<()> {
    let home = get_hermes_home();
    let db_path = home.join("sessions.db");

    println!();
    println!("{}", cyan().apply_to("◆ Hermes Insights"));
    println!();

    if !db_path.exists() {
        println!("  {}", yellow().apply_to("No sessions database found. Start chatting to generate insights."));
        println!();
        return Ok(());
    }

    let db = match hermes_state::SessionDB::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            println!("  {} Could not open sessions DB: {}", yellow().apply_to("⚠"), e);
            println!();
            return Ok(());
        }
    };

    // Total sessions
    let total_sessions = db.session_count(None).unwrap_or(0);
    println!("  {:20} {}", "Total Sessions", total_sessions);

    if total_sessions == 0 {
        println!();
        println!("  {}", dim().apply_to("No sessions yet. Start chatting to see insights."));
        println!();
        return Ok(());
    }

    // Total messages
    let total_messages = db.message_count(None).unwrap_or(0);
    println!("  {:20} {}", "Total Messages", total_messages);

    // Average messages per session
    let avg_msgs = if total_sessions > 0 {
        total_messages as f64 / total_sessions as f64
    } else {
        0.0
    };
    println!("  {:20} {:.1}", "Avg Messages/Session", avg_msgs);

    // Source breakdown
    println!();
    println!("  {}", cyan().apply_to("By Source:"));
    for source in &["cli", "telegram", "discord", "slack", "whatsapp", "signal", "sms", "matrix", "mattermost", "homeassistant", "bluebubbles", "wecom_callback"] {
        if let Ok(count) = db.session_count(Some(source)) {
            if count > 0 {
                println!("    {:20} {}", source, count);
            }
        }
    }

    // Token usage + Top models (single query)
    println!();
    println!("  {}", cyan().apply_to("Token Usage (estimated):"));
    if let Ok(sessions) = db.list_sessions_rich(None, None, 1000, 0, false) {
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        let mut cost: f64 = 0.0;
        let mut model_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for swp in &sessions {
            input_tokens += swp.session.input_tokens;
            output_tokens += swp.session.output_tokens;
            cost += swp.session.estimated_cost_usd.unwrap_or(0.0);
            if let Some(model) = &swp.session.model {
                *model_counts.entry(model.clone()).or_insert(0) += 1;
            }
        }
        println!("  {:20} {}", "Input Tokens", format_tokens(input_tokens));
        println!("  {:20} {}", "Output Tokens", format_tokens(output_tokens));
        println!("  {:20} ${:.4}", "Estimated Cost", cost);

        println!();
        println!("  {}", cyan().apply_to("Top Models:"));
        let mut sorted: Vec<_> = model_counts.into_iter().collect();
        sorted.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        for (model, count) in sorted.iter().take(5) {
            println!("    {:30} {} sessions", model, count);
        }
    }

    println!();
    Ok(())
}

fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn test_format_tokens_kilo() {
        assert_eq!(format_tokens(5_000), "5.0K");
    }

    #[test]
    fn test_format_tokens_mega() {
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn test_insights_runs_without_errors() {
        let result = cmd_insights(30, None);
        assert!(result.is_ok());
    }
}
