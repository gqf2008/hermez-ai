//! Hermez String Plugin — text processing utilities.
//!
//! Tools:
//!   string_reverse  → reverse a string
//!   string_count    → count chars, words, or lines
//!   string_replace  → replace occurrences of a substring
//!   string_split    → split by delimiter
//!   string_case     → convert case (upper, lower, title, snake, camel, kebab)
//!   string_trim     → trim whitespace / prefix / suffix
//!   string_base64   → encode or decode base64

#[allow(warnings)]
mod bindings;

use bindings::exports::hermez::plugin::plugin::Guest;
use bindings::hermez::plugin::host;

struct StringPlugin;

impl Guest for StringPlugin {
    fn register() {
        host::log("info", "String plugin registered — provides text processing tools");
    }

    fn on_session_start(_ctx: String) {}
    fn on_session_end(_ctx: String) {}

    fn handle_tool(name: String, args: String) -> Result<String, String> {
        match name.as_str() {
            "string_reverse" => handle_reverse(&args),
            "string_count" => handle_count(&args),
            "string_replace" => handle_replace(&args),
            "string_split" => handle_split(&args),
            "string_case" => handle_case(&args),
            "string_trim" => handle_trim(&args),
            "string_base64" => handle_base64(&args),
            _ => Err(format!("Unknown tool: '{}'", name)),
        }
    }
}

// ── JSON helpers (same pattern as time-plugin) ───────────────────────────────

fn get_json_str(args: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let start = args.find(&pattern)? + pattern.len();
    let rest = &args[start..];
    let rest = rest.trim_start();
    let rest = if rest.starts_with(':') { &rest[1..] } else { rest };
    let rest = rest.trim_start();
    if rest.starts_with('"') {
        let rest = &rest[1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else {
        let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn get_json_bool(args: &str, key: &str) -> Option<bool> {
    let v = get_json_str(args, key)?;
    Some(v == "true")
}

// ── Tool Handlers ────────────────────────────────────────────────────────────

fn handle_reverse(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let reversed: String = text.chars().rev().collect();
    Ok(format!(
        "{{\"original\":\"{}\",\"reversed\":\"{}\"}}",
        json_escape(&text),
        json_escape(&reversed)
    ))
}

fn handle_count(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let mode = get_json_str(args, "mode").unwrap_or_else(|| "chars".to_string());

    let count = match mode.as_str() {
        "words" => text.split_whitespace().count(),
        "lines" => text.lines().count(),
        "chars" | _ => text.chars().count(),
    };

    Ok(format!(
        "{{\"text\":\"{}\",\"mode\":\"{}\",\"count\":{}}}",
        json_escape(&text),
        mode,
        count
    ))
}

fn handle_replace(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let from = get_json_str(args, "from").ok_or("Missing 'from'")?;
    let to = get_json_str(args, "to").ok_or("Missing 'to'")?;
    let all = get_json_bool(args, "all").unwrap_or(true);

    let result = if all {
        text.replace(&from, &to)
    } else {
        match text.find(&from) {
            Some(i) => {
                let mut r = text.clone();
                r.replace_range(i..i + from.len(), &to);
                r
            }
            None => text.clone(),
        }
    };

    Ok(format!(
        "{{\"original\":\"{}\",\"result\":\"{}\",\"replacements\":{}}}",
        json_escape(&text),
        json_escape(&result),
        if all {
            text.matches(&from).count()
        } else {
            text.find(&from).map(|_| 1).unwrap_or(0)
        }
    ))
}

fn handle_split(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let delim = get_json_str(args, "delimiter").unwrap_or_else(|| " ".to_string());
    let limit = get_json_str(args, "limit")
        .and_then(|s| s.parse::<usize>().ok());

    let parts: Vec<String> = if let Some(lim) = limit {
        text.splitn(lim, &delim).map(|s| s.to_string()).collect()
    } else {
        text.split(&delim).map(|s| s.to_string()).collect()
    };

    let parts_json: Vec<String> = parts.iter().map(|p| format!("\"{}\"", json_escape(p))).collect();
    Ok(format!(
        "{{\"count\":{},\"parts\":[{}]}}",
        parts_json.len(),
        parts_json.join(",")
    ))
}

fn handle_case(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let mode = get_json_str(args, "mode").ok_or("Missing 'mode'")?;

    let result = match mode.as_str() {
        "upper" => text.to_uppercase(),
        "lower" => text.to_lowercase(),
        "title" => to_title_case(&text),
        "snake" => to_snake_case(&text),
        "camel" => to_camel_case(&text),
        "kebab" => to_kebab_case(&text),
        _ => return Err(format!("Unknown case mode: '{}'", mode)),
    };

    Ok(format!(
        "{{\"original\":\"{}\",\"mode\":\"{}\",\"result\":\"{}\"}}",
        json_escape(&text),
        mode,
        json_escape(&result)
    ))
}

fn handle_trim(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let mode = get_json_str(args, "mode").unwrap_or_else(|| "both".to_string());

    let result = match mode.as_str() {
        "left" => text.trim_start().to_string(),
        "right" => text.trim_end().to_string(),
        "both" | _ => text.trim().to_string(),
    };

    Ok(format!(
        "{{\"original\":\"{}\",\"mode\":\"{}\",\"result\":\"{}\"}}",
        json_escape(&text),
        mode,
        json_escape(&result)
    ))
}

fn handle_base64(args: &str) -> Result<String, String> {
    let text = get_json_str(args, "text").ok_or("Missing 'text'")?;
    let op = get_json_str(args, "operation").unwrap_or_else(|| "encode".to_string());

    let result = match op.as_str() {
        "encode" => base64_encode(text.as_bytes()),
        "decode" => base64_decode(&text)?,
        _ => return Err(format!("Unknown operation: '{}'", op)),
    };

    Ok(format!(
        "{{\"operation\":\"{}\",\"result\":\"{}\"}}",
        op,
        json_escape(&result)
    ))
}

// ── Case conversion helpers ──────────────────────────────────────────────────

fn to_title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_uppercase() {
            if prev_lower {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap_or(c));
            prev_lower = false;
        } else if c.is_alphanumeric() {
            result.push(c);
            prev_lower = c.is_lowercase();
        } else if c.is_whitespace() {
            result.push('_');
            prev_lower = false;
        }
    }
    result
}

fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if capitalize_next {
                result.push(c.to_uppercase().next().unwrap_or(c));
                capitalize_next = false;
            } else {
                result.push(c);
            }
        } else {
            capitalize_next = true;
        }
    }
    // Lowercase first char
    if let Some(first) = result.chars().next() {
        let rest: String = result.chars().skip(1).collect();
        return first.to_lowercase().collect::<String>() + &rest;
    }
    result
}

fn to_kebab_case(s: &str) -> String {
    to_snake_case(s).replace('_', "-")
}

// ── Base64 ───────────────────────────────────────────────────────────────────

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 2 < data.len() {
        let b = ((data[i] as usize) << 16) | ((data[i + 1] as usize) << 8) | (data[i + 2] as usize);
        result.push(B64[(b >> 18) & 0x3F] as char);
        result.push(B64[(b >> 12) & 0x3F] as char);
        result.push(B64[(b >> 6) & 0x3F] as char);
        result.push(B64[b & 0x3F] as char);
        i += 3;
    }
    if i < data.len() {
        let b = if i + 1 < data.len() {
            ((data[i] as usize) << 16) | ((data[i + 1] as usize) << 8)
        } else {
            (data[i] as usize) << 16
        };
        result.push(B64[(b >> 18) & 0x3F] as char);
        result.push(B64[(b >> 12) & 0x3F] as char);
        if i + 1 < data.len() {
            result.push(B64[(b >> 6) & 0x3F] as char);
        } else {
            result.push('=');
        }
        result.push('=');
    }
    result
}

fn base64_decode(s: &str) -> Result<String, String> {
    let mut table = [0u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }

    let mut buf = Vec::with_capacity(s.len() / 4 * 3);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let b0 = if bytes[i] == b'=' { 0 } else { table[bytes[i] as usize] };
        let b1 = if bytes[i + 1] == b'=' { 0 } else { table[bytes[i + 1] as usize] };
        let b2 = if bytes[i + 2] == b'=' { 0 } else { table[bytes[i + 2] as usize] };
        let b3 = if bytes[i + 3] == b'=' { 0 } else { table[bytes[i + 3] as usize] };
        buf.push((b0 << 2) | (b1 >> 4));
        if bytes[i + 2] != b'=' {
            buf.push((b1 << 4) | (b2 >> 2));
        }
        if bytes[i + 3] != b'=' {
            buf.push((b2 << 6) | b3);
        }
        i += 4;
    }
    String::from_utf8(buf).map_err(|e| format!("Invalid UTF-8 after decode: {}", e))
}

// ── Misc ─────────────────────────────────────────────────────────────────────

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

bindings::export!(StringPlugin with_types_in bindings);
