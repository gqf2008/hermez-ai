//! Advanced input handling for Hermez CLI.
//!
//! Multiline editing, input validation, and prompt customization
//! for the reedline-based interactive shell.

use console::{style, Style};

/// Input mode for the prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// Single-line input (default).
    Single,
    /// Multi-line input with Ctrl+J for newline.
    Multi,
}

/// Result from reading user input.
pub struct InputResult {
    /// The text the user entered.
    pub text: String,
    /// Whether the user wants to submit (Enter) vs cancel (Ctrl+C).
    pub submitted: bool,
}

/// Read a line of input with a styled prompt.
pub fn read_input(prompt: &str) -> std::io::Result<String> {
    use std::io::{self, Write};

    print!("{}", style(prompt).cyan().bold());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_string())
}

/// Read multiline input until the user enters a blank line or sends EOF.
pub fn read_multiline(prompt: &str) -> std::io::Result<String> {
    use std::io;

    let dim = Style::new().dim();
    println!("{}", style(prompt).cyan().bold());
    println!("  {} (end with blank line or Ctrl+D)", dim.apply_to("Enter text:"));

    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() && !lines.is_empty() {
                    break;
                }
                lines.push(trimmed.to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(lines.join("\n"))
}

/// Build a reedline prompt string with status indicators.
pub fn build_prompt(
    model: &str,
    token_count: Option<usize>,
    is_voice: bool,
    budget_remaining: Option<f64>,
) -> String {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let red = Style::new().red();
    let dim = Style::new().dim();

    let short_model = model.split('/').next_back().unwrap_or(model);

    let mut parts = vec![
        green.apply_to(format!("[{short_model}]")).to_string(),
    ];

    if is_voice {
        parts.push(yellow.apply_to("VOICE").to_string());
    }

    if let Some(tokens) = token_count {
        let display = if tokens > 100_000 {
            red.apply_to(format!("{:.0}kt", tokens as f64 / 1000.0)).to_string()
        } else {
            dim.apply_to(format!("{tokens}t")).to_string()
        };
        parts.push(display);
    }

    if let Some(budget) = budget_remaining {
        let display = if budget < 0.10 {
            red.apply_to(format!("${budget:.2}")).to_string()
        } else {
            dim.apply_to(format!("${budget:.2}")).to_string()
        };
        parts.push(display);
    }

    parts.join(" ") + " > "
}

/// Validate and clean up user input.
pub fn validate_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip invisible Unicode characters
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !matches!(*c as u32, 0x200B | 0x200C | 0x200D | 0xFEFF))
        .collect();

    if cleaned.is_empty() {
        return None;
    }

    Some(cleaned)
}

/// Parse slash commands from user input.
pub fn parse_command(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let rest = &trimmed[1..];
    if let Some(space) = rest.find(' ') {
        Some((&rest[..space], rest[space + 1..].trim()))
    } else {
        Some((rest, ""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_input_empty() {
        assert!(validate_input("").is_none());
        assert!(validate_input("   ").is_none());
        assert!(validate_input("\t\n").is_none());
    }

    #[test]
    fn test_validate_input_clean() {
        let result = validate_input("hello world");
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn test_validate_input_unicode() {
        let result = validate_input("hello\u{200B}world");
        assert_eq!(result, Some("helloworld".to_string()));
    }

    #[test]
    fn test_parse_command_none() {
        assert!(parse_command("hello world").is_none());
    }

    #[test]
    fn test_parse_command_simple() {
        let (cmd, args) = parse_command("/reset").unwrap();
        assert_eq!(cmd, "reset");
        assert_eq!(args, "");
    }

    #[test]
    fn test_parse_command_with_args() {
        let (cmd, args) = parse_command("/model anthropic/opus").unwrap();
        assert_eq!(cmd, "model");
        assert_eq!(args, "anthropic/opus");
    }

    #[test]
    fn test_parse_command_extra_spaces() {
        let (cmd, args) = parse_command("  /tools  ").unwrap();
        assert_eq!(cmd, "tools");
        assert_eq!(args, "");
    }

    #[test]
    fn test_build_prompt_basic() {
        let prompt = build_prompt("anthropic/claude-opus-4.6", None, false, None);
        assert!(prompt.contains("claude-opus-4.6"));
        assert!(prompt.ends_with("> "));
    }

    #[test]
    fn test_build_prompt_with_tokens() {
        let prompt = build_prompt("openai/gpt-4o", Some(5000), false, None);
        assert!(prompt.contains("5000t"));
    }

    #[test]
    fn test_build_prompt_voice() {
        let prompt = build_prompt("anthropic/opus", None, true, None);
        assert!(prompt.contains("VOICE"));
    }

    #[test]
    fn test_build_prompt_all_indicators() {
        let prompt = build_prompt("anthropic/opus", Some(150000), true, Some(0.50));
        assert!(prompt.contains("opus"));
        assert!(prompt.contains("VOICE"));
        assert!(prompt.contains("150kt"));
        assert!(prompt.contains("$0.50"));
    }
}
