#![allow(dead_code)]
//! Fuzzy string matching tool.
//!
//! Mirrors the Python `tools/fuzzy_match.py` (482 lines).
//! Pure algorithmic logic — no external deps, no terminal, no async.
//!
//! Provides fuzzy matching with edit distance, scoring, and candidate ranking.

use std::sync::LazyLock;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::{self, ToolRegistry};
use hermez_core::Result;

/// Cached matcher instance — stateless and reusable.
static MATCHER: LazyLock<SkimMatcherV2> = LazyLock::new(SkimMatcherV2::default);

/// Input schema for the fuzzy_match tool.
pub fn fuzzy_match_schema() -> Value {
    serde_json::json!({
        "name": "fuzzy_match",
        "description": "Fuzzy match a query string against a list of candidates with scoring and ranking.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The query string to match against candidates."
                },
                "candidates": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of candidate strings to match."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return.",
                    "default": 10,
                    "minimum": 1,
                    "maximum": 100
                },
                "threshold": {
                    "type": "integer",
                    "description": "Minimum score threshold (negative values allowed).",
                    "default": 0
                }
            },
            "required": ["query", "candidates"]
        }
    })
}

/// A single fuzzy match result.
#[derive(Debug, Serialize, Deserialize)]
pub struct FuzzyMatchResult {
    /// The matched candidate string.
    pub candidate: String,
    /// The match score (higher is better).
    pub score: i64,
    /// The index in the original candidate list.
    pub index: usize,
}

/// Perform fuzzy matching on a query against a list of candidates.
///
/// Uses the SkimMatcherV2 algorithm (same approach as Python's fuzzy_match.py).
pub fn fuzzy_match(query: &str, candidates: &[String], limit: usize, threshold: i64) -> Vec<FuzzyMatchResult> {
    let mut results: Vec<FuzzyMatchResult> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, candidate)| {
            MATCHER.fuzzy_match(candidate, query).map(|score| FuzzyMatchResult {
                candidate: candidate.clone(),
                score,
                index: i,
            })
        })
        .filter(|r| r.score >= threshold)
        .collect();

    // Sort by score descending
    results.sort_by(|a, b| b.score.cmp(&a.score));

    // Take top N
    results.truncate(limit);
    results
}

/// Handler for the fuzzy_match tool.
pub fn handle_fuzzy_match(args: Value) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| hermez_core::HermezError::new(
            hermez_core::errors::ErrorCategory::ToolError,
            "fuzzy_match requires 'query' parameter (string)",
        ))?;

    let candidates = args["candidates"]
        .as_array()
        .ok_or_else(|| hermez_core::HermezError::new(
            hermez_core::errors::ErrorCategory::ToolError,
            "fuzzy_match requires 'candidates' parameter (array of strings)",
        ))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Ok(registry::tool_error("'candidates' must be a non-empty array of strings"));
    }

    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
    let threshold = args["threshold"].as_i64().unwrap_or(0);

    let results = fuzzy_match(query, &candidates, limit, threshold);

    registry::tool_result(serde_json::json!({
        "query": query,
        "results": results,
        "total_matches": results.len(),
        "total_candidates": candidates.len(),
    }))
}

/// Register the fuzzy_match tool in the registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(
        "fuzzy_match".to_string(),
        "organization".to_string(),
        fuzzy_match_schema(),
        std::sync::Arc::new(handle_fuzzy_match),
        None, // No availability check
        vec![], // No env vars required
        "Fuzzy match a query against candidates with scoring".to_string(),
        "🔍".to_string(),
        None,
    );
}

/// Fuzzy find-and-replace using similarity scoring.
///
/// Returns (new_content, match_count, strategy_used) or (original_content, 0, "") if no match.
///
/// This mirrors the Python `fuzzy_find_and_replace` which handles whitespace
/// normalization, indentation differences, and block-anchor matching.
pub fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    // Try exact match first
    let exact_count = content.matches(old_string).count();
    if exact_count > 0 {
        let result = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        return (result, if replace_all { exact_count } else { 1 }, "exact");
    }

    // Strategy 2: line_trimmed — trim each line's leading/trailing whitespace
    let result = line_trimmed_fuzzy_replace(content, old_string, new_string, replace_all);
    if result.1 > 0 { return result; }

    // Strategy 3: whitespace_normalize + similarity
    let normalize = |s: &str| {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let norm_old = normalize(old_string);
    let norm_content = normalize(content);

    if norm_content.contains(&norm_old) {
        return fuzzy_replace_by_similarity(content, old_string, new_string, replace_all);
    }

    // Strategy 4: indentation_flexible — strip all leading indentation
    let result = indentation_flexible_fuzzy_replace(content, old_string, new_string, replace_all);
    if result.1 > 0 { return result; }

    // Strategy 5: escape_normalized — convert \n \t \r literals to actual chars
    let result = escape_normalized_fuzzy_replace(content, old_string, new_string, replace_all);
    if result.1 > 0 { return result; }

    // Strategy 6: trimmed_boundary — trim empty leading/trailing lines
    let result = trimmed_boundary_fuzzy_replace(content, old_string, new_string, replace_all);
    if result.1 > 0 { return result; }

    // Strategy 7: unicode_normalized — NFC + lookalike normalization
    let result = unicode_normalized_fuzzy_replace(content, old_string, new_string, replace_all);
    if result.1 > 0 { return result; }

    // Strategy 8: block-anchor fuzzy matching
    block_anchor_fuzzy_replace(content, old_string, new_string, replace_all)
}

/// Strategy 2: line_trimmed — normalize each line by trimming whitespace.
fn line_trimmed_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    let trim_lines = |s: &str| -> Vec<String> {
        s.lines().map(|l| l.trim().to_string()).collect()
    };
    let norm_content = trim_lines(content);
    let norm_old = trim_lines(old_string);
    let norm_new = trim_lines(new_string);

    if norm_old.is_empty() {
        return (content.to_string(), 0, "");
    }

    // Find all matching blocks
    let mut matches = Vec::new();
    let mut i = 0;
    while i <= norm_content.len().saturating_sub(norm_old.len()) {
        if norm_content[i..i + norm_old.len()] == norm_old[..] {
            matches.push((i, i + norm_old.len()));
            if !replace_all {
                break;
            }
            i += norm_old.len();
        } else {
            i += 1;
        }
    }

    if matches.is_empty() {
        return (content.to_string(), 0, "");
    }

    // Replace in reverse order to preserve positions
    let content_lines: Vec<&str> = content.lines().collect();
    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    for (start, end) in matches.iter().rev() {
        let replacement = if norm_new.len() == 1 {
            // Preserve the indentation of the first original line
            let first_orig = content_lines[*start];
            let indent = first_orig.chars().take_while(|c| c.is_whitespace()).collect::<String>();
            norm_new[0].lines().map(|line| {
                format!("{indent}{line}")
            }).collect::<Vec<_>>().join("\n")
        } else {
            norm_new.join("\n")
        };
        let _range_len = end - start;
        result_lines.splice(*start..*end, std::iter::once(replacement).flat_map(|s| {
            s.split('\n').map(String::from).collect::<Vec<_>>()
        }));
    }

    (result_lines.join("\n"), matches.len(), "line_trimmed")
}

/// Strategy 4: indentation_flexible — strip leading indent uniformly.
fn indentation_flexible_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    let strip_indent = |s: &str| -> Vec<String> {
        let min_indent = s.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        s.lines()
            .map(|l| l.chars().skip(min_indent).collect())
            .collect()
    };
    let norm_content = strip_indent(content);
    let norm_old = strip_indent(old_string);

    if norm_old.is_empty() {
        return (content.to_string(), 0, "");
    }

    // Search for norm_old as contiguous block in norm_content
    let mut matches = Vec::new();
    let mut i = 0;
    while i <= norm_content.len().saturating_sub(norm_old.len()) {
        if norm_content[i..i + norm_old.len()] == norm_old[..] {
            matches.push((i, i + norm_old.len()));
            if !replace_all { break; }
            i += norm_old.len();
        } else {
            i += 1;
        }
    }

    if matches.is_empty() {
        return (content.to_string(), 0, "");
    }

    // Replace: detect content's base indent and apply to new_string
    let content_lines: Vec<&str> = content.lines().collect();
    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    for (start, end) in matches.iter().rev() {
        let first_orig = content_lines[*start];
        let indent = first_orig.chars().take_while(|c| c.is_whitespace()).collect::<String>();
        let replacement = new_string.lines()
            .map(|l| format!("{indent}{l}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _range_len = end - start;
        result_lines.splice(*start..*end, std::iter::once(replacement).flat_map(|s| {
            s.split('\n').map(String::from).collect::<Vec<_>>()
        }));
    }

    (result_lines.join("\n"), matches.len(), "indentation_flexible")
}

/// Strategy 5: escape_normalized — convert literal escape sequences.
fn escape_normalized_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    let normalize_escapes = |s: &str| -> String {
        s.replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\r", "\r")
    };
    let norm_content = normalize_escapes(content);
    let norm_old = normalize_escapes(old_string);

    if norm_old.is_empty() || !norm_content.contains(&norm_old) {
        return (content.to_string(), 0, "");
    }

    let count = norm_content.matches(&norm_old).count();
    let norm_new = normalize_escapes(new_string);
    let result = if replace_all {
        norm_content.replace(&norm_old, &norm_new)
    } else {
        norm_content.replacen(&norm_old, &norm_new, 1)
    };

    (result, if replace_all { count } else { 1 }, "escape_normalized")
}

/// Strategy 6: trimmed_boundary — trim empty leading/trailing lines.
fn trimmed_boundary_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    // Trim leading/trailing empty lines from old_string only
    let norm_old = old_string
        .lines()
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let norm_old = norm_old
        .rsplit('\n')
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    if norm_old.is_empty() {
        return (content.to_string(), 0, "");
    }

    if !content.contains(&norm_old) {
        return (content.to_string(), 0, "");
    }

    let count = content.matches(&norm_old).count();
    let result = if replace_all {
        content.replace(&norm_old, new_string)
    } else {
        content.replacen(&norm_old, new_string, 1)
    };

    (result, if replace_all { count } else { 1 }, "trimmed_boundary")
}

/// Strategy 7: unicode_normalized — NFC + lookalike character normalization.
fn unicode_normalized_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, &'static str) {
    use unicode_normalization::UnicodeNormalization;

    // Normalize lookalike characters
    const LOOKALIKES: &[(char, &str)] = &[
        ('\u{201C}', "\""), // "
        ('\u{201D}', "\""), // "
        ('\u{2018}', "'"),  // '
        ('\u{2019}', "'"),  // '
        ('\u{2013}', "-"),  // –
        ('\u{2014}', "--"), // —
        ('\u{2026}', "..."), // …
        ('\u{00A0}', " "),  // non-breaking space
        ('\u{FF01}', "!"),  // ！
        ('\u{FF0C}', ","),  // ，
        ('\u{FF1A}', ":"),  // ：
        ('\u{FF1B}', ";"),  // ；
        ('\u{FF1F}', "?"),  // ？
    ];

    let normalize_unicode = |s: &str| -> String {
        let mut result = s.nfc().collect::<String>();
        for &(orig, repl) in LOOKALIKES {
            result = result.replace(orig, repl);
        }
        result
    };

    let norm_content = normalize_unicode(content);
    let norm_old = normalize_unicode(old_string);

    if norm_old.is_empty() || !norm_content.contains(&norm_old) {
        return (content.to_string(), 0, "");
    }

    let count = norm_content.matches(&norm_old).count();
    let norm_new = normalize_unicode(new_string);
    let result = if replace_all {
        norm_content.replace(&norm_old, &norm_new)
    } else {
        norm_content.replacen(&norm_old, &norm_new, 1)
    };

    (result, if replace_all { count } else { 1 }, "unicode_normalized")
}

/// Try to match using line-by-line similarity with similar crate.
fn fuzzy_replace_by_similarity(
    content: &str,
    old_string: &str,
    new_string: &str,
    _replace_all: bool,
) -> (String, usize, &'static str) {
    use similar::{ChangeTag, TextDiff};

    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() || content_lines.is_empty() {
        return (content.to_string(), 0, "");
    }

    let diff = TextDiff::from_slices(&old_lines, &content_lines);

    // Find the best contiguous block match — look for regions where most
    // changes are Keep or Replace (not Insert).
    let _changes: Vec<_> = diff.iter_all_changes().collect();
    let old_len = old_lines.len();

    // Sliding window: find a region in content that matches old_string best
    let mut best_start = None;
    let mut best_score = 0f64;
    let mut best_end = 0;

    for window_start in 0..content_lines.len().saturating_sub(old_len / 2) {
        let window_end = (window_start + old_len).min(content_lines.len());
        if window_start >= window_end { continue; }

        let window: &[&str] = &content_lines[window_start..window_end];
        let wdiff = TextDiff::from_slices(&old_lines, window);
        let mut keeps = 0;
        let mut total = 0;
        for change in wdiff.iter_all_changes() {
            if change.tag() != ChangeTag::Insert {
                keeps += 1;
            }
            if change.tag() != ChangeTag::Delete {
                total += 1;
            }
        }
        let score = if total > 0 { keeps as f64 / old_len as f64 } else { 0.0 };
        if score > best_score {
            best_score = score;
            best_start = Some(window_start);
            best_end = window_end;
        }
    }

    let threshold = 0.5;
    if best_score >= threshold {
        if let Some(start) = best_start {
            let end = best_end.min(content_lines.len());
            let before = content_lines[..start].join("\n");
            let after = content_lines[end..].join("\n");
            let result = if before.is_empty() {
                format!("{new_string}\n{after}")
            } else if after.is_empty() {
                format!("{before}\n{new_string}")
            } else {
                format!("{before}\n{new_string}\n{after}")
            };
            return (result, 1, "similarity");
        }
    }

    (content.to_string(), 0, "")
}

/// Block-anchor fuzzy matching: match by finding lines that appear in both
/// old_string and content, then use those as anchors.
fn block_anchor_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    _replace_all: bool,
) -> (String, usize, &'static str) {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    // Find first and last non-empty lines as anchors
    let first_anchor = old_lines.iter().find(|l| !l.trim().is_empty());
    let last_anchor = old_lines.iter().rev().find(|l| !l.trim().is_empty());

    let (Some(first), Some(last)) = (first_anchor, last_anchor) else {
        return (content.to_string(), 0, "");
    };

    let first_pos = content_lines.iter().position(|l| l.trim() == first.trim());
    let last_pos = first_pos.and_then(|fp| {
        content_lines[fp..].iter().position(|l| l.trim() == last.trim()).map(|lp| fp + lp)
    });

    if let (Some(start), Some(end)) = (first_pos, last_pos) {
        let before = content_lines[..start].join("\n");
        let after = if end + 1 < content_lines.len() {
            content_lines[end + 1..].join("\n")
        } else {
            String::new()
        };
        let result = if before.is_empty() {
            format!("{new_string}\n{after}")
        } else if after.is_empty() {
            format!("{before}\n{new_string}")
        } else {
            format!("{before}\n{new_string}\n{after}")
        };
        return (result, 1, "block-anchor");
    }

    (content.to_string(), 0, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_basic() {
        let candidates = vec![
            "hello world".to_string(),
            "foo bar".to_string(),
            "hello there".to_string(),
        ];

        let results = fuzzy_match("hlo wrld", &candidates, 10, 0);
        assert!(!results.is_empty());
        // "hello world" should match with a higher score than "foo bar"
        assert_eq!(results[0].candidate, "hello world");
    }

    #[test]
    fn test_fuzzy_match_limit() {
        let candidates: Vec<_> = (0..20).map(|i| format!("item {i}")).collect();
        let results = fuzzy_match("item", &candidates, 3, 0);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_fuzzy_match_threshold() {
        let candidates = vec![
            "exact match".to_string(),
            "completely different".to_string(),
        ];

        let results_low = fuzzy_match("exact", &candidates, 10, 0);
        let results_high = fuzzy_match("exact", &candidates, 10, 100);

        // Lower threshold should match more candidates
        assert!(results_low.len() >= results_high.len());
    }

    #[test]
    fn test_fuzzy_match_no_match() {
        let candidates = vec!["xyz".to_string(), "abc".to_string()];
        let results = fuzzy_match("qqq", &candidates, 10, 100);
        // May or may not match depending on score, but with high threshold should be empty
        assert!(results.is_empty() || results[0].score < 100);
    }

    #[test]
    fn test_handler_missing_param() {
        let result = handle_fuzzy_match(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_line_trimmed_match() {
        // Different indentation but same content per line
        let content = "fn hello() {\n    println!(\"hi\");\n}";
        let old = "  fn hello() {\n      println!(\"hi\");\n  }";
        let new_str = "fn hello() {\n    println!(\"hello\");\n}";
        let (result, count, strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert_eq!(count, 1);
        assert_eq!(strategy, "line_trimmed");
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_indentation_flexible_match() {
        // Content has uniform indent but old_string has a different base indent
        // line_trimmed won't match here because content has different per-line indent
        let content = "    def foo():\n        pass\n    # done";
        let old = "def foo():\n  pass";
        let new_str = "def bar():\n  return None";
        let (result, count, _strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert!(count >= 1);
        assert!(result.contains("bar"));
    }

    #[test]
    fn test_escape_normalized_match() {
        let content = "line1\nline2\nline3";
        let old = "line1\\nline2";
        let new_str = "LINE1\\nLINE2";
        let (result, count, strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert_eq!(count, 1);
        assert_eq!(strategy, "escape_normalized");
        assert!(result.contains("LINE1"));
    }

    #[test]
    fn test_trimmed_boundary_match() {
        // Old string has extra blank lines around the target
        let content = "header\nfunc main() {}\nfooter";
        let old = "\n\nfunc main() {}\n";
        let new_str = "fn main() {}";
        let (result, count, _strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert_eq!(count, 1);
        assert!(result.contains("fn main()"));
    }

    #[test]
    fn test_unicode_normalized_match() {
        // Smart quotes instead of straight quotes
        let content = r#"let x = "hello";"#;
        let old = "let x = \u{201C}hello\u{201D};";
        let new_str = "let x = \"world\";";
        let (result, count, strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert_eq!(count, 1);
        assert_eq!(strategy, "unicode_normalized");
        assert!(result.contains("world"));
    }

    #[test]
    fn test_strategy_order_exact_first() {
        // Exact match should always be first strategy
        let content = "fn test() {}";
        let old = "fn test() {}";
        let new_str = "fn test() { /* modified */ }";
        let (result, count, strategy) = fuzzy_find_and_replace(content, old, new_str, false);
        assert_eq!(count, 1);
        assert_eq!(strategy, "exact");
        assert!(result.contains("modified"));
    }

    #[test]
    fn test_replace_all_line_trimmed() {
        let content = "  foo\n  foo\n  foo";
        let old = "foo\nfoo\nfoo";
        let new_str = "bar";
        let (result, count, strategy) = fuzzy_find_and_replace(content, old, new_str, true);
        assert_eq!(count, 1); // line_trimmed finds one contiguous block
        assert_eq!(strategy, "line_trimmed");
        assert!(result.contains("bar"));
    }
}
