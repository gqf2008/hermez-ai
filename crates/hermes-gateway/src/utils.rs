//! Shared utility functions for the gateway crate.

/// Truncate text to a maximum number of Unicode characters.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Truncate text to a maximum number of Unicode characters, appending a suffix
/// when truncation occurs.
pub fn truncate_text_with_suffix(text: &str, max_chars: usize, suffix: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let take = max_chars.saturating_sub(suffix.chars().count());
    format!("{}{}", text.chars().take(take).collect::<String>(), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_text() {
        assert_eq!(truncate_text("hello", 10), "hello");
        let long = "a".repeat(100);
        assert_eq!(truncate_text(&long, 10).chars().count(), 10);
    }

    #[test]
    fn test_truncate_text_utf8_safe() {
        let text = "Hello 😀 World";
        assert_eq!(truncate_text(text, 3), "Hel");
        assert_eq!(truncate_text(text, 7), "Hello 😀");
        assert_eq!(truncate_text(text, 100), text);
    }

    #[test]
    fn test_truncate_text_with_suffix() {
        assert_eq!(truncate_text_with_suffix("hello", 10, "..."), "hello");
        assert_eq!(truncate_text_with_suffix("hello world", 5, "..."), "he...");
        assert_eq!(truncate_text_with_suffix("1234567890abcdef", 10, "..."), "1234567...");
    }

    #[test]
    fn test_truncate_text_with_suffix_utf8_safe() {
        let text = "Hello 😀 World";
        assert_eq!(truncate_text_with_suffix(text, 8, "..."), "Hello...");
        assert_eq!(truncate_text_with_suffix(text, 100, "..."), text);
    }
}
