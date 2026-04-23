#![allow(dead_code)]
//! User-facing summaries for manual compression commands.
//!
//! Mirrors the Python `agent/manual_compression_feedback.py`.

/// Format a number with comma separators (e.g. 5000 → "5,000").
fn format_number(n: usize) -> String {
    n.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(",")
}

/// Result of summarizing a manual compression operation.
pub struct CompressionSummary {
    /// Whether the compression resulted in no changes.
    pub noop: bool,
    /// Headline describing the compression result.
    pub headline: String,
    /// Token count line (before → after).
    pub token_line: String,
    /// Optional note explaining counterintuitive results.
    pub note: Option<String>,
}

/// Return consistent user-facing feedback for manual compression.
pub fn summarize_manual_compression(
    before_count: usize,
    after_count: usize,
    before_tokens: usize,
    after_tokens: usize,
) -> CompressionSummary {
    let noop = before_count == after_count;

    let (headline, token_line) = if noop {
        let headline = format!("No changes from compression: {before_count} messages");
        let token_line = if after_tokens == before_tokens {
            format!("Rough transcript estimate: ~{} tokens (unchanged)", format_number(before_tokens))
        } else {
            format!("Rough transcript estimate: ~{} → ~{} tokens", format_number(before_tokens), format_number(after_tokens))
        };
        (headline, token_line)
    } else {
        let headline = format!("Compressed: {before_count} → {after_count} messages");
        let token_line =
            format!("Rough transcript estimate: ~{} → ~{} tokens", format_number(before_tokens), format_number(after_tokens));
        (headline, token_line)
    };

    let note = if !noop && after_count < before_count && after_tokens > before_tokens {
        Some(
            "Note: fewer messages can still raise this rough transcript estimate \
             when compression rewrites the transcript into denser summaries."
                .to_string(),
        )
    } else {
        None
    };

    CompressionSummary {
        noop,
        headline,
        token_line,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_unchanged_tokens() {
        let result = summarize_manual_compression(10, 10, 5000, 5000);
        assert!(result.noop);
        assert!(result.headline.contains("No changes"));
        assert!(result.token_line.contains("unchanged"));
        assert!(result.note.is_none());
    }

    #[test]
    fn test_noop_different_tokens() {
        let result = summarize_manual_compression(10, 10, 5000, 4800);
        assert!(result.noop);
        assert!(result.token_line.contains("5,000 → ~4,800"));
        assert!(result.note.is_none());
    }

    #[test]
    fn test_compressed_fewer_messages() {
        let result = summarize_manual_compression(20, 10, 8000, 4000);
        assert!(!result.noop);
        assert!(result.headline.contains("20 → 10"));
        assert!(result.token_line.contains("8,000 → ~4,000"));
        assert!(result.note.is_none());
    }

    #[test]
    fn test_compressed_fewer_messages_more_tokens() {
        let result = summarize_manual_compression(20, 10, 5000, 6000);
        assert!(!result.noop);
        assert!(result.note.is_some());
        let note = result.note.unwrap();
        assert!(note.contains("denser summaries"));
    }
}
