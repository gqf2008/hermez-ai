#![allow(dead_code)]
//! V4A Patch Format Parser.
//!
//! Parses the V4A patch format used by codex, cline, and other coding agents.
//! Mirrors the Python `tools/patch_parser.py`.

use serde::Serialize;

/// Type of patch operation.
#[derive(Debug, Clone, PartialEq)]
pub enum OperationType {
    Add,
    Update,
    Delete,
    Move,
}

/// A single line in a patch hunk.
#[derive(Debug, Clone)]
pub struct HunkLine {
    pub prefix: char, // ' ', '-', or '+'
    pub content: String,
}

/// A group of changes within a file.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub context_hint: Option<String>,
    pub lines: Vec<HunkLine>,
}

/// A single operation in a V4A patch.
#[derive(Debug, Clone)]
pub struct PatchOperation {
    pub operation: OperationType,
    pub file_path: String,
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// Result of applying a patch.
#[derive(Debug, Serialize)]
pub struct PatchResult {
    pub success: bool,
    pub diff: String,
    pub files_modified: Vec<String>,
    pub files_created: Vec<String>,
    pub files_deleted: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Parse a V4A format patch.
///
/// Returns `(operations, None)` on success, or `([], Some(error))` on failure.
pub fn parse_v4a_patch(patch_content: &str) -> (Vec<PatchOperation>, Option<String>) {
    let lines: Vec<&str> = patch_content.split('\n').collect();

    // Find patch boundaries
    let mut start_idx: Option<usize> = None;
    let mut end_idx: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        if line.contains("*** Begin Patch") || line.contains("***Begin Patch") {
            start_idx = Some(i);
        } else if line.contains("*** End Patch") || line.contains("***End Patch") {
            end_idx = Some(i);
            break;
        }
    }

    let start = start_idx.map(|i| i + 1).unwrap_or(0);
    let end = end_idx.unwrap_or(lines.len());

    // Guard: if End Patch appeared before Begin Patch, skip the pre-marker content
    let start = if start > end { end } else { start };

    let mut operations: Vec<PatchOperation> = Vec::new();
    let mut current_op: Option<PatchOperation> = None;
    let mut current_hunk: Option<Hunk> = None;

    for line in lines.iter().take(end).skip(start) {
        let line = *line;

        // Check for file operation markers
        if let Some(path) = capture_match(line, "*** Update File:") {
            flush_operation(&mut current_op, &mut current_hunk, &mut operations);
            current_op = Some(PatchOperation {
                operation: OperationType::Update,
                file_path: path,
                new_path: None,
                hunks: Vec::new(),
            });
            current_hunk = None;
        } else if let Some(path) = capture_match(line, "*** Add File:") {
            flush_operation(&mut current_op, &mut current_hunk, &mut operations);
            current_op = Some(PatchOperation {
                operation: OperationType::Add,
                file_path: path,
                new_path: None,
                hunks: Vec::new(),
            });
            current_hunk = Some(Hunk {
                context_hint: None,
                lines: Vec::new(),
            });
        } else if let Some(path) = capture_match(line, "*** Delete File:") {
            flush_operation(&mut current_op, &mut current_hunk, &mut operations);
            operations.push(PatchOperation {
                operation: OperationType::Delete,
                file_path: path,
                new_path: None,
                hunks: Vec::new(),
            });
            current_op = None;
            current_hunk = None;
        } else if let Some((old, new)) = capture_move(line) {
            flush_operation(&mut current_op, &mut current_hunk, &mut operations);
            operations.push(PatchOperation {
                operation: OperationType::Move,
                file_path: old,
                new_path: Some(new),
                hunks: Vec::new(),
            });
            current_op = None;
            current_hunk = None;
        } else if line.starts_with("@@") {
            // Context hint / hunk marker
            if let Some(op) = &mut current_op {
                if current_hunk.as_ref().is_some_and(|h| !h.lines.is_empty()) {
                    let hunk = current_hunk.take().unwrap();
                    op.hunks.push(hunk);
                }
                let hint = extract_hint(line);
                current_hunk = Some(Hunk {
                    context_hint: hint,
                    lines: Vec::new(),
                });
            }
        } else if !line.is_empty() {
            // Hunk line
            if current_op.is_some() {
                if current_hunk.is_none() {
                    current_hunk = Some(Hunk {
                        context_hint: None,
                        lines: Vec::new(),
                    });
                }
                if let Some(hunk) = &mut current_hunk {
                    if let Some(stripped) = line.strip_prefix('+') {
                        hunk.lines.push(HunkLine {
                            prefix: '+',
                            content: stripped.to_string(),
                        });
                    } else if let Some(stripped) = line.strip_prefix('-') {
                        hunk.lines.push(HunkLine {
                            prefix: '-',
                            content: stripped.to_string(),
                        });
                    } else if let Some(stripped) = line.strip_prefix(' ') {
                        hunk.lines.push(HunkLine {
                            prefix: ' ',
                            content: stripped.to_string(),
                        });
                    } else if line.starts_with('\\') {
                        // "\ No newline at end of file" — skip
                    } else {
                        // Treat as context line (implicit space prefix)
                        hunk.lines.push(HunkLine {
                            prefix: ' ',
                            content: line.to_string(),
                        });
                    }
                }
            }
        }
    }

    // Flush last operation
    flush_operation(&mut current_op, &mut current_hunk, &mut operations);

    // Validate the parsed result
    validate_patch(&operations)
}

/// Validate the parsed patch operations.
///
/// Returns `(operations, None)` on success, or `([], Some(error))` on failure.
/// Empty patch is not an error — callers get `[]` and can decide.
fn validate_patch(operations: &[PatchOperation]) -> (Vec<PatchOperation>, Option<String>) {
    if operations.is_empty() {
        return (Vec::new(), None);
    }

    let mut parse_errors: Vec<String> = Vec::new();

    for op in operations {
        if op.file_path.is_empty() {
            parse_errors.push("Operation with empty file path".to_string());
        }
        if op.operation == OperationType::Update && op.hunks.is_empty() {
            parse_errors.push(format!("UPDATE {:?}: no hunks found", op.file_path));
        }
        if op.operation == OperationType::Move && op.new_path.is_none() {
            parse_errors.push(format!(
                "MOVE {:?}: missing destination path (expected 'src -> dst')",
                op.file_path
            ));
        }
    }

    if parse_errors.is_empty() {
        (operations.to_vec(), None)
    } else {
        (
            Vec::new(),
            Some(format!("Parse error: {}", parse_errors.join("; "))),
        )
    }
}

fn capture_match(line: &str, prefix: &str) -> Option<String> {
    let line_lower = line.trim().to_lowercase();
    let prefix_lower = prefix.to_lowercase();
    if line_lower.starts_with(&prefix_lower) {
        // Use the lowercase prefix length to slice the original line.
        // Safe because all marker prefixes are ASCII-only.
        let path = line.trim()[prefix_lower.len()..].trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    None
}

fn capture_move(line: &str) -> Option<(String, String)> {
    let line_trimmed = line.trim();
    let line_lower = line_trimmed.to_lowercase();
    let prefix = "*** move file:";
    if line_lower.starts_with(prefix) {
        let rest = line_trimmed[prefix.len()..].trim();
        if let Some((old, new)) = rest.split_once("->") {
            let old = old.trim().to_string();
            let new = new.trim().to_string();
            if !old.is_empty() && !new.is_empty() {
                return Some((old, new));
            }
        }
    }
    None
}

fn extract_hint(line: &str) -> Option<String> {
    let stripped = line.trim();
    if stripped.starts_with("@@") && stripped.ends_with("@@") && stripped.len() > 4 {
        let inner = stripped[2..stripped.len() - 2].trim();
        if !inner.is_empty() {
            return Some(inner.to_string());
        }
    }
    None
}

fn flush_operation(
    current_op: &mut Option<PatchOperation>,
    current_hunk: &mut Option<Hunk>,
    operations: &mut Vec<PatchOperation>,
) {
    if let Some(mut op) = current_op.take() {
        if let Some(hunk) = current_hunk.take() {
            if !hunk.lines.is_empty() {
                op.hunks.push(hunk);
            }
        }
        operations.push(op);
    }
}

/// Format a single patch operation for display.
pub fn format_operation(op: &PatchOperation) -> String {
    match op.operation {
        OperationType::Add => format!("Add File: {}", op.file_path),
        OperationType::Delete => format!("Delete File: {}", op.file_path),
        OperationType::Move => format!(
            "Move File: {} -> {}",
            op.file_path,
            op.new_path.as_deref().unwrap_or("?")
        ),
        OperationType::Update => {
            let hunk_count = op.hunks.len();
            format!("Update File: {} ({} hunks)", op.file_path, hunk_count)
        }
    }
}

/// Count non-overlapping occurrences of *pattern* in *text*.
fn count_occurrences(text: &str, pattern: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = text[start..].find(pattern) {
        count += 1;
        start += pos + pattern.len();
    }
    count
}

/// Validate a single addition-only hunk's context hint against file content.
///
/// Returns error if the context hint is not found or is ambiguous (appears >1 times).
pub fn validate_addition_hint(content: &str, hunk: &Hunk) -> Option<String> {
    if let Some(ref hint) = hunk.context_hint {
        let occurrences = count_occurrences(content, hint);
        if occurrences == 0 {
            return Some(format!(
                "addition-only hunk context hint '{}' not found",
                hint
            ));
        }
        if occurrences > 1 {
            return Some(format!(
                "addition-only hunk context hint '{}' is ambiguous ({} occurrences)",
                hint, occurrences
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_add_file() {
        let patch = r#"*** Begin Patch
*** Add File: hello.txt
+hello world
+second line
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Add);
        assert_eq!(ops[0].file_path, "hello.txt");
        assert_eq!(ops[0].hunks[0].lines.len(), 2);
        assert_eq!(ops[0].hunks[0].lines[0].prefix, '+');
        assert_eq!(ops[0].hunks[0].lines[0].content, "hello world");
    }

    #[test]
    fn test_parse_update_with_hunks() {
        let patch = r#"*** Begin Patch
*** Update File: main.py
@@ def hello @@
 def hello():
-    print("old")
+    print("new")
     return True
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Update);
        assert_eq!(ops[0].file_path, "main.py");
        assert_eq!(ops[0].hunks.len(), 1);
        let hunk = &ops[0].hunks[0];
        assert_eq!(hunk.context_hint, Some("def hello".to_string()));
        assert_eq!(hunk.lines.len(), 4);
    }

    #[test]
    fn test_parse_delete_file() {
        let patch = r#"*** Begin Patch
*** Delete File: old.txt
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Delete);
        assert_eq!(ops[0].file_path, "old.txt");
    }

    #[test]
    fn test_parse_move_file() {
        let patch = r#"*** Begin Patch
*** Move File: old.py -> new.py
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Move);
        assert_eq!(ops[0].file_path, "old.py");
        assert_eq!(ops[0].new_path, Some("new.py".to_string()));
    }

    #[test]
    fn test_parse_multiple_operations() {
        let patch = r#"*** Begin Patch
*** Add File: new.txt
+content
*** Delete File: old.txt
*** Update File: main.py
@@ func @@
-old
+new
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].operation, OperationType::Add);
        assert_eq!(ops[1].operation, OperationType::Delete);
        assert_eq!(ops[2].operation, OperationType::Update);
    }

    #[test]
    fn test_format_operation() {
        let op = PatchOperation {
            operation: OperationType::Update,
            file_path: "test.py".to_string(),
            new_path: None,
            hunks: vec![Hunk {
                context_hint: None,
                lines: Vec::new(),
            }],
        };
        assert_eq!(format_operation(&op), "Update File: test.py (1 hunks)");
    }

    #[test]
    fn test_empty_patch_is_not_error() {
        let patch = "*** Begin Patch\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert!(ops.is_empty());
    }

    #[test]
    fn test_validate_update_missing_hunks() {
        let patch = r#"*** Begin Patch
*** Update File: main.py
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_some());
        assert!(err.as_ref().unwrap().contains("Parse error"));
        assert!(err.as_ref().unwrap().contains("no hunks found"));
        assert!(ops.is_empty());
    }

    #[test]
    fn test_validate_move_missing_destination() {
        // A move without a "->" would be parsed as an update with no hunks
        // since capture_move requires the "->" separator
        let patch = r#"*** Begin Patch
*** Move File: old.py
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        // This won't match capture_move since there's no "->"
        // It gets parsed as an empty op or nothing
        assert!(ops.is_empty() || err.is_some());
    }

    #[test]
    fn test_validate_valid_update_with_hunks() {
        let patch = r#"*** Begin Patch
*** Update File: main.py
@@ func @@
-old
+new
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].hunks.len(), 1);
    }

    #[test]
    fn test_validate_move_valid() {
        let patch = r#"*** Begin Patch
*** Move File: old.py -> new.py
*** End Patch"#;
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Move);
        assert_eq!(ops[0].new_path, Some("new.py".to_string()));
    }

    #[test]
    fn test_count_occurrences() {
        assert_eq!(count_occurrences("hello world hello", "hello"), 2);
        assert_eq!(count_occurrences("abc", "xyz"), 0);
        assert_eq!(count_occurrences("aaa", "aa"), 1); // non-overlapping
        assert_eq!(count_occurrences("", "x"), 0);
    }

    #[test]
    fn test_validate_addition_hint_found() {
        let content = "fn main() {\n    let x = 1;\n}";
        let hunk = Hunk {
            context_hint: Some("fn main()".to_string()),
            lines: vec![HunkLine { prefix: '+', content: "let y = 2;".to_string() }],
        };
        assert!(validate_addition_hint(content, &hunk).is_none());
    }

    #[test]
    fn test_validate_addition_hint_not_found() {
        let content = "fn foo() {}";
        let hunk = Hunk {
            context_hint: Some("fn bar()".to_string()),
            lines: vec![HunkLine { prefix: '+', content: "x".to_string() }],
        };
        let err = validate_addition_hint(content, &hunk).unwrap();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_validate_addition_hint_ambiguous() {
        let content = "fn main() {}\n// more\ndef fn main() {}\n// more";
        let hunk = Hunk {
            context_hint: Some("fn main()".to_string()),
            lines: vec![HunkLine { prefix: '+', content: "x".to_string() }],
        };
        let err = validate_addition_hint(content, &hunk).unwrap();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("2 occurrences"));
    }

    #[test]
    fn test_validate_addition_no_hint() {
        let content = "any content";
        let hunk = Hunk {
            context_hint: None,
            lines: vec![HunkLine { prefix: '+', content: "x".to_string() }],
        };
        assert!(validate_addition_hint(content, &hunk).is_none());
    }
}
