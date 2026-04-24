//! Text utilities shared across the workspace.

/// Strip reasoning/thinking blocks from content.
///
/// Handles all tag variants: `<think>`, `<thinking>`, `<reasoning>`,
/// `<REASONING_SCRATCHPAD>`, `<thought>`, `<|think|>`, etc.
/// Case-insensitive for `<thinking>` and `<thought>` tags.
pub fn strip_think_blocks(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let len = content.len();
    let mut i = 0;

    while i < len {
        let rest = &content[i..];

        // <|think|>...|>
        if rest.starts_with("<|think|>") {
            if let Some(end) = rest[9..].find("|>") {
                i += end + 11;
                continue;
            } else {
                break;
            }
        }

        // <think>...</think>
        if rest.starts_with("<think>") {
            if let Some(end) = rest.find("</think>") {
                i += end + 8;
                continue;
            } else {
                break;
            }
        }

        // <thinking>...</thinking> (case-insensitive)
        if find_tag(rest, "<thinking>") == Some(0) {
            if let Some(end) = find_tag(rest, "</thinking>") {
                i += end + 11;
                continue;
            } else {
                break;
            }
        }

        // <reasoning>...</reasoning>
        if rest.starts_with("<reasoning>") {
            if let Some(end) = rest.find("</reasoning>") {
                i += end + 12;
                continue;
            } else {
                break;
            }
        }

        // <REASONING_SCRATCHPAD>...</REASONING_SCRATCHPAD>
        if rest.starts_with("<REASONING_SCRATCHPAD>") {
            if let Some(end) = rest.find("</REASONING_SCRATCHPAD>") {
                i += end + 23;
                continue;
            } else {
                break;
            }
        }

        // <thought>...</thought> (case-insensitive)
        if find_tag(rest, "<thought>") == Some(0) {
            if let Some(end) = find_tag(rest, "</thought>") {
                i += end + 10;
                continue;
            } else {
                break;
            }
        }

        // Strip bare closing tags that leaked through
        if rest.starts_with("</think>")
            || find_tag(rest, "</thinking>") == Some(0)
            || rest.starts_with("</reasoning>")
            || find_tag(rest, "</thought>") == Some(0)
            || rest.starts_with("</REASONING_SCRATCHPAD>")
        {
            if let Some(gt) = rest.find('>') {
                i += gt + 1;
                continue;
            }
        }

        // Not inside a think block — emit complete UTF-8 character
        let c = rest.chars().next().unwrap();
        let char_len = c.len_utf8();
        result.push(c);
        i += char_len;
    }

    result
}

/// ASCII case-insensitive substring search.
///
/// Tags are ASCII, so byte offsets in the original string are preserved.
fn find_tag(haystack: &str, tag: &str) -> Option<usize> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len();
    let first = tag_bytes[0].to_ascii_lowercase();
    haystack
        .as_bytes()
        .windows(tag_len)
        .enumerate()
        .find(|(_, window)| {
            window[0].to_ascii_lowercase() == first
                && window
                    .iter()
                    .zip(tag_bytes)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_think_blocks_basic() {
        let input = "Hello <think>thinking</think> world";
        assert_eq!(strip_think_blocks(input), "Hello  world");
    }

    #[test]
    fn test_strip_thinking_case_insensitive() {
        let input = "Before <ThInKiNg>secret</THINKING> after";
        assert_eq!(strip_think_blocks(input), "Before  after");
    }

    #[test]
    fn test_strip_reasoning_block() {
        let input = "Start <reasoning>deep thought</reasoning> end";
        assert_eq!(strip_think_blocks(input), "Start  end");
    }

    #[test]
    fn test_strip_reasoning_scratchpad() {
        let input = "A <REASONING_SCRATCHPAD>scratch</REASONING_SCRATCHPAD> B";
        assert_eq!(strip_think_blocks(input), "A  B");
    }

    #[test]
    fn test_strip_thought_case_insensitive() {
        let input = "X <Thought>idea</THOUGHT> Y";
        assert_eq!(strip_think_blocks(input), "X  Y");
    }

    #[test]
    fn test_strip_think_pipe_delimited() {
        let input = "A <|think|>inner|> B";
        assert_eq!(strip_think_blocks(input), "A  B");
    }

    #[test]
    fn test_strip_bare_closing_tags() {
        let input = "Text </think> more";
        assert_eq!(strip_think_blocks(input), "Text  more");
    }

    #[test]
    fn test_strip_multiple_blocks() {
        let input = "A <think>1</think> B <think>2</think> C";
        assert_eq!(strip_think_blocks(input), "A  B  C");
    }

    #[test]
    fn test_no_think_blocks_passthrough() {
        let input = "Just plain text with <b>html</b> tags.";
        assert_eq!(strip_think_blocks(input), input);
    }

    #[test]
    fn test_unclosed_think_block_strips_to_end() {
        let input = "Start <think>never closed";
        assert_eq!(strip_think_blocks(input), "Start ");
    }

    #[test]
    fn test_unclosed_thinking_block_strips_to_end() {
        let input = "Start <thinking>never closed";
        assert_eq!(strip_think_blocks(input), "Start ");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(strip_think_blocks(""), "");
    }

    #[test]
    fn test_unicode_preserved() {
        let input = "Hello 🌍 <think>hidden</think> World";
        assert_eq!(strip_think_blocks(input), "Hello 🌍  World");
    }

    #[test]
    fn test_find_tag_basic() {
        assert_eq!(find_tag("abc<THINKING>def", "<thinking>"), Some(3));
    }

    #[test]
    fn test_find_tag_not_found() {
        assert_eq!(find_tag("abc", "<thinking>"), None);
    }
}
