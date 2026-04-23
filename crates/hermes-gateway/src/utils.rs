//! Shared utility functions for the gateway crate.

/// Truncate text to a maximum number of Unicode characters.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
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
}
