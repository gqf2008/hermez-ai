#![allow(dead_code)]
//! Strip ANSI escape sequences from subprocess output.
//!
//! Mirrors the Python `tools/ansi_strip.py`.
//! Uses a two-pass approach: fast-path check + full regex.

use once_cell::sync::Lazy;
use regex::Regex;

/// Check if text contains any escape byte (fast path).
fn has_escape_byte(text: &str) -> bool {
    text.bytes().any(|b| b == 0x1b || (0x80..=0x9f).contains(&b))
}

/// Full ANSI escape sequence regex.
static ANSI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\x1b(?:\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\][^\x07]*(?:\x07|\x1b\\)|[PX^_][^\x1b]*(?:\x1b\\)|[\x20-\x2f][\x30-\x7e]|[\x30-\x7e])|\x9b[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x9d[^\x07]*(?:\x07|\x9c)|[\x80-\x9f]").expect("static ANSI regex is valid")
});

/// Strip all ANSI escape sequences from the given text.
///
/// Returns the text unchanged if no escape bytes are present (fast path).
pub fn strip_ansi(text: &str) -> String {
    if !has_escape_byte(text) {
        return text.to_string();
    }
    ANSI_RE.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_escape() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_simple_color() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn test_cursor_movement() {
        assert_eq!(strip_ansi("text\x1b[2Jmore"), "textmore");
    }

    #[test]
    fn test_osc_title() {
        assert_eq!(strip_ansi("\x1b]0;title\x07hello"), "hello");
    }

    #[test]
    fn test_mixed() {
        let input = "\x1b[1m\x1b[32mOK\x1b[0m";
        assert_eq!(strip_ansi(input), "OK");
    }

    #[test]
    fn test_empty() {
        assert_eq!(strip_ansi(""), "");
    }
}
