//! Curses-based TUI components for Hermes CLI.
//!
//! Interactive menus for model selection, file picking, and session browsing.
//!
//! Mirrors Python `hermes_cli/curses_ui.py` using the `dialoguer` crate for
//! cross-platform terminal interaction (replacing Python's stdlib `curses`).

use console::{style, Style};
use std::io::{self, IsTerminal, Write};

// ---------------------------------------------------------------------------
// Stdin flush — prevents escape-sequence leakage after interactive prompts
// ---------------------------------------------------------------------------

/// Flush any stray bytes from the stdin input buffer.
///
/// Must be called after `dialoguer` (or any terminal-mode library) returns,
/// **before** the next `input()` / readline call.  Restoring the terminal
/// does NOT drain the OS input buffer — leftover escape-sequence bytes
/// (from arrow keys, terminal mode-switch responses, or rapid keypresses)
/// remain buffered and silently get consumed by the next read call,
/// corrupting user data (e.g. writing `^[^[` into .env files).
///
/// On non-TTY stdin (piped, redirected) or Windows, this is a no-op.
pub fn flush_stdin() {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return;
        }
        unsafe {
            // TCIFLUSH = flush data received but not read
            let _ = libc::tcflush(stdin.as_raw_fd(), libc::TCIFLUSH);
        }
    }
    #[cfg(not(unix))]
    {
        // Windows does not have a direct tcflush equivalent accessible
        // without extra crates.  The risk is lower on Windows because
        // console mode switching is handled differently.
        let _ = io::stdin();
    }
}

// ---------------------------------------------------------------------------
// Checklist (multi-select)
// ---------------------------------------------------------------------------

/// Display an interactive multi-select checklist and return selected indices.
///
/// Args:
/// - `title`: Header prompt displayed above the checklist.
/// - `items`: Display labels for each row.
/// - `selected`: Indices that start checked (pre-selected).
/// - `cancel_returns`: Value returned when the user cancels (ESC/q).
///   Defaults to a clone of `selected`.
///
/// On non-TTY stdin (piped, redirected), returns `cancel_returns` immediately.
pub fn show_checklist(
    title: &str,
    items: &[String],
    selected: &[usize],
    cancel_returns: Option<Vec<usize>>,
) -> Vec<usize> {
    let fallback = cancel_returns.unwrap_or_else(|| selected.to_vec());

    if items.is_empty() || !io::stdin().is_terminal() {
        return fallback;
    }

    let defaults: Vec<bool> = (0..items.len())
        .map(|i| selected.contains(&i))
        .collect();

    let result = dialoguer::MultiSelect::new()
        .with_prompt(title)
        .items(items)
        .defaults(&defaults)
        .interact();

    flush_stdin();

    match result {
        Ok(chosen) => chosen,
        Err(_) => fallback,
    }
}

// ---------------------------------------------------------------------------
// Radio list (single-select with radio buttons)
// ---------------------------------------------------------------------------

/// Display an interactive single-select radio list and return the selected index.
///
/// Args:
/// - `title`: Header prompt.
/// - `items`: Display labels.
/// - `selected`: Index that starts selected.
/// - `cancel_returns`: Value returned on cancel. Defaults to `selected`.
///
/// On non-TTY stdin, returns `cancel_returns` immediately.
pub fn show_radiolist(
    title: &str,
    items: &[String],
    selected: usize,
    cancel_returns: Option<usize>,
) -> usize {
    let fallback = cancel_returns.unwrap_or(selected);

    if items.is_empty() || !io::stdin().is_terminal() {
        return fallback;
    }

    let default_idx = selected.min(items.len().saturating_sub(1));

    let result = dialoguer::Select::new()
        .with_prompt(title)
        .items(items)
        .default(default_idx)
        .interact();

    flush_stdin();

    match result {
        Ok(idx) => idx,
        Err(_) => fallback,
    }
}

// ---------------------------------------------------------------------------
// Single-select menu (with cancel option)
// ---------------------------------------------------------------------------

/// Display an interactive single-select menu. Returns `Some(index)` on confirm
/// or `None` if the user cancels.
///
/// Args:
/// - `title`: Header prompt.
/// - `items`: Display labels.
/// - `default_index`: Index that starts selected.
/// - `cancel_label`: Label appended at the end of the list for cancellation.
///
/// On non-TTY stdin, returns `None` immediately.
pub fn show_single_select(
    title: &str,
    items: &[String],
    default_index: usize,
    cancel_label: &str,
) -> Option<usize> {
    if items.is_empty() || !io::stdin().is_terminal() {
        return None;
    }

    let mut all_items: Vec<String> = items.to_vec();
    all_items.push(cancel_label.to_string());
    let cancel_idx = items.len();

    let default_idx = default_index.min(all_items.len().saturating_sub(1));

    let result = dialoguer::Select::new()
        .with_prompt(title)
        .items(&all_items)
        .default(default_idx)
        .interact();

    flush_stdin();

    match result {
        Ok(idx) if idx == cancel_idx => None,
        Ok(idx) => Some(idx),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Simple numbered menu (fallback / non-interactive terminal)
// ---------------------------------------------------------------------------

/// Display a numbered menu and return the selected index.
///
/// Uses keyboard navigation: numbered selection, Enter to confirm,
/// q to cancel.  This is a lightweight fallback for terminals where
/// `dialoguer` is not desired or unavailable.
pub fn show_menu(
    title: &str,
    items: &[String],
    allow_cancel: bool,
) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }

    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    println!("\n{} {}", green.apply_to(">"), title);
    println!("{}", dim.apply_to(&"-".repeat(title.len() + 2)));

    for (i, item) in items.iter().enumerate() {
        println!("  {} {}", yellow.apply_to(format!("{:>2}.", i + 1)), item);
    }
    println!();

    if allow_cancel {
        print!("Select (1-{}/q): ", items.len());
    } else {
        print!("Select (1-{}): ", items.len());
    }
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("q") {
        return Ok(None);
    }

    match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= items.len() => Ok(Some(n - 1)),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Display helpers (non-interactive)
// ---------------------------------------------------------------------------

/// Display available models grouped by provider.
pub fn show_model_selector(models: &[String], current: Option<&str>) {
    let green = Style::new().green();
    let dim = Style::new().dim();

    println!("\n{} Available Models", green.apply_to(">"));

    if let Some(cur) = current {
        println!("  {} Current: {}", dim.apply_to("->"), cur);
    }
    println!();

    let mut providers: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for model in models {
        if let Some((provider, rest)) = model.split_once('/') {
            providers
                .entry(provider.to_string())
                .or_default()
                .push(rest.to_string());
        } else {
            providers
                .entry("other".to_string())
                .or_default()
                .push(model.clone());
        }
    }

    for (provider, names) in &providers {
        println!("  {}", style(provider).bold());
        for name in names {
            let full = format!("{provider}/{name}");
            let marker = if current == Some(full.as_str()) {
                green.apply_to(" (current)").to_string()
            } else {
                String::new()
            };
            println!("    - {name}{marker}");
        }
    }
}

/// Display a session browser showing recent sessions.
pub fn show_session_list(sessions: &[(String, String, String)]) {
    // (id, title, last_message_preview)
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    println!("\n{} Recent Sessions", green.apply_to(">"));
    println!("{}", dim.apply_to(&"-".repeat(50)));

    if sessions.is_empty() {
        println!("  {}", dim.apply_to("No sessions found."));
        return;
    }

    for (id, title, preview) in sessions {
        let short_id = if id.len() > 8 { &id[..8] } else { id };
        println!(
            "  {} {}  {}",
            yellow.apply_to(short_id),
            style(title).bold(),
            dim.apply_to(preview),
        );
    }
}

/// Display a skill browser with categories.
pub fn show_skills_list(categories: &[(String, Vec<(String, String)>)]) {
    // (category_name, [(skill_name, description)])
    let green = Style::new().green();
    let dim = Style::new().dim();

    println!("\n{} Skills", green.apply_to(">"));

    for (cat, skills) in categories {
        println!("\n  {}", style(cat).bold().underlined());
        if skills.is_empty() {
            println!("    {}", dim.apply_to("(none)"));
        }
        for (name, desc) in skills {
            println!("    {}  {}", style(name).cyan(), dim.apply_to(desc));
        }
    }
}

/// Display a confirmation prompt with colored default.
pub fn confirm(prompt_text: &str, default: bool) -> io::Result<bool> {
    let dim = Style::new().dim();

    let default_label = if default { "Y/n" } else { "y/N" };
    print!("{} [{}]: ", prompt_text, dim.apply_to(default_label));
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input.is_empty() {
        return Ok(default);
    }

    Ok(input == "y" || input == "yes")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_show_menu_empty() {
        let result = show_menu("Empty", &[], true).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_show_checklist_empty() {
        let result = show_checklist("Empty", &[], &[], None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_show_checklist_fallback() {
        let items = vec!["a".to_string(), "b".to_string()];
        let result = show_checklist("Test", &items, &[], Some(vec![0]));
        // On non-TTY (test env) returns fallback
        assert_eq!(result, vec![0]);
    }

    #[test]
    fn test_show_radiolist_empty() {
        let result = show_radiolist("Empty", &[], 0, None);
        assert_eq!(result, 0);
    }

    #[test]
    fn test_show_radiolist_fallback() {
        let items = vec!["a".to_string(), "b".to_string()];
        let result = show_radiolist("Test", &items, 0, Some(1));
        // On non-TTY (test env) returns fallback
        assert_eq!(result, 1);
    }

    #[test]
    fn test_show_single_select_empty() {
        let result = show_single_select("Empty", &[], 0, "Cancel");
        assert!(result.is_none());
    }

    #[test]
    fn test_show_single_select_fallback() {
        let items = vec!["a".to_string(), "b".to_string()];
        let result = show_single_select("Test", &items, 0, "Cancel");
        // On non-TTY (test env) returns None
        assert!(result.is_none());
    }

    #[test]
    fn test_show_session_list_empty() {
        show_session_list(&[]);
    }

    #[test]
    fn test_show_skills_list_empty() {
        show_skills_list(&[]);
    }

    #[test]
    fn test_show_model_selector_empty() {
        show_model_selector(&[], None);
    }

    #[test]
    fn test_flush_stdin_does_not_panic() {
        // Should never panic, even on Windows or non-TTY.
        flush_stdin();
    }
}
