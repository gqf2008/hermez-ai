//! Reedline completers for Hermes CLI.
//!
//! Provides tab-completion for commands, model names, file paths,
//! and skill names within the interactive chat session.

use reedline::{Completer, Span, Suggestion};
use std::path::Path;

/// Completer for Hermes CLI slash commands.
pub struct CommandCompleter {
    commands: Vec<&'static str>,
}

impl CommandCompleter {
    pub fn new() -> Self {
        Self {
            commands: vec![
                "/help", "/quit", "/exit", "/clear", "/reset", "/tools",
                "/skills", "/models", "/model", "/doctor", "/setup",
                "/compress", "/voice", "/budget", "/sessions", "/session",
                "/profiles", "/profile",
            ],
        }
    }
}

impl Default for CommandCompleter {
    fn default() -> Self {
        Self::new()
    }
}

impl Completer for CommandCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let word_start = line[..pos].rfind(' ').map(|p| p + 1).unwrap_or(0);
        let word = &line[word_start..pos];

        if !word.starts_with('/') {
            return Vec::new();
        }

        self.commands
            .iter()
            .filter(|&&cmd| cmd.starts_with(word))
            .map(|&cmd| Suggestion {
                value: cmd.to_string(),
                description: None,
                style: None,
                extra: None,
                span: Span::new(word_start, pos),
                append_whitespace: true,
            })
            .collect()
    }
}

/// Completer for model names.
pub struct ModelCompleter {
    models: Vec<String>,
}

impl ModelCompleter {
    pub fn new(models: Vec<String>) -> Self {
        Self { models }
    }

    /// Build from common model prefixes.
    pub fn from_env() -> Self {
        let mut models = Vec::new();
        for (prefix, vars) in &[
            ("anthropic/", &["ANTHROPIC_API_KEY"]),
            ("openai/", &["OPENAI_API_KEY"]),
            ("openrouter/", &["OPENROUTER_API_KEY"]),
            ("gemini/", &["GEMINI_API_KEY"]),
            ("groq/", &["GROQ_API_KEY"]),
        ] {
            for var in *vars {
                if std::env::var(var).is_ok() {
                    models.push(prefix.to_string());
                    break;
                }
            }
        }
        if models.is_empty() {
            models.push("anthropic/claude-opus-4.6".to_string());
        }
        Self { models }
    }
}

impl Completer for ModelCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let word_start = line[..pos].rfind(' ').map(|p| p + 1).unwrap_or(0);
        let word = &line[word_start..pos];

        // Only complete after /model command
        if !line.starts_with("/model ") && !line.starts_with("/models ") {
            return Vec::new();
        }

        self.models
            .iter()
            .filter(|m| m.to_lowercase().starts_with(&word.to_lowercase()))
            .map(|m| Suggestion {
                value: m.clone(),
                description: None,
                style: None,
                extra: None,
                span: Span::new(word_start, pos),
                append_whitespace: true,
            })
            .collect()
    }
}

/// Completer for file paths.
pub struct FileCompleter;

impl FileCompleter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileCompleter {
    fn default() -> Self {
        Self::new()
    }
}

impl Completer for FileCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let word_start = line[..pos].rfind(' ').map(|p| p + 1).unwrap_or(0);
        let word = &line[word_start..pos];

        // Expand tilde
        let expanded = if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let home_str = home.to_string_lossy();
            if let Some(rest) = word.strip_prefix("~/") {
                format!("{home_str}/{rest}")
            } else if word == "~" {
                home_str.to_string()
            } else {
                word.to_string()
            }
        } else {
            word.to_string()
        };
        let path = Path::new(&expanded);

        let (search_dir, prefix) = if path.is_dir() {
            (expanded.as_str(), "")
        } else if let Some(parent) = path.parent() {
            (parent.to_str().unwrap_or("."), path.file_name().unwrap_or_default().to_str().unwrap_or(""))
        } else {
            (".", word)
        };

        let mut suggestions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(search_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                if let Ok(name) = entry.file_name().into_string() {
                    if name.starts_with(prefix) && !name.starts_with('.') {
                        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                        let display_name = if is_dir {
                            format!("{name}/")
                        } else {
                            name.clone()
                        };
                        suggestions.push(Suggestion {
                            value: display_name,
                            description: None,
                            style: None,
                            extra: None,
                            span: Span::new(word_start, pos),
                            append_whitespace: false,
                        });
                    }
                }
            }
        }

        suggestions.sort_by(|a, b| a.value.cmp(&b.value));
        suggestions
    }
}

/// Combined completer that chains multiple completers.
pub struct HermesCompleter {
    command: CommandCompleter,
    file: FileCompleter,
}

impl HermesCompleter {
    pub fn new() -> Self {
        Self {
            command: CommandCompleter::new(),
            file: FileCompleter::new(),
        }
    }
}

impl Default for HermesCompleter {
    fn default() -> Self {
        Self::new()
    }
}

impl Completer for HermesCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        // Try file completion first if line contains path-like patterns
        let word_start = line[..pos].rfind(' ').map(|p| p + 1).unwrap_or(0);
        let word = &line[word_start..pos];
        if word.starts_with('~') || word.starts_with('.') || word.starts_with('/') || word.contains('/') {
            let results = self.file.complete(line, pos);
            if !results.is_empty() {
                return results;
            }
        }

        // Fall back to command completion
        if line.starts_with('/') || line.is_empty() {
            self.command.complete(line, pos)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_completer_empty_line() {
        let mut completer = CommandCompleter::new();
        let results = completer.complete("/", 1);
        // "/" matches all commands
        assert!(!results.is_empty());
    }

    #[test]
    fn test_command_completer_partial() {
        let mut completer = CommandCompleter::new();
        let results = completer.complete("/mod", 4);
        assert!(!results.is_empty());
        assert!(results.iter().all(|s| s.value.starts_with("/mod")));
    }

    #[test]
    fn test_command_completer_no_slash() {
        let mut completer = CommandCompleter::new();
        let results = completer.complete("hello", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_model_completer() {
        let mut completer = ModelCompleter::new(vec!["anthropic/opus".to_string(), "openai/gpt-4".to_string()]);
        let results = completer.complete("/model anth", 11);
        assert!(!results.is_empty());
        assert!(results.iter().any(|s| s.value == "anthropic/opus"));
    }

    #[test]
    fn test_file_completer_no_match() {
        let mut completer = FileCompleter::new();
        let results = completer.complete("/xyznonexistent", 15);
        // Non-existent directory — should return empty
        assert!(results.is_empty());
    }

    #[test]
    fn test_combined_completer() {
        let mut completer = HermesCompleter::new();
        // "/" matches commands
        let results = completer.complete("/", 1);
        assert!(!results.is_empty());

        // "/t" matches tool-related commands
        let results = completer.complete("/t", 2);
        assert!(!results.is_empty());
    }
}
