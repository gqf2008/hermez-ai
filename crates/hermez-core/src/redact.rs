#![allow(dead_code)]
//! Regex-based secret redaction for logs and tool output.
//!
//! Applies pattern matching to mask API keys, tokens, and credentials
//! before they reach log files, verbose output, or gateway logs.
//!
//! Short tokens (< 18 chars) are fully masked. Longer tokens preserve
//! the first 6 and last 4 characters for debuggability.

use once_cell::sync::Lazy;
use regex::Regex;

/// Whether redaction is enabled. Snapshot at import time so runtime env
/// mutations cannot disable redaction mid-session.
static REDACT_ENABLED: Lazy<bool> = Lazy::new(|| {
    let val = std::env::var("HERMEZ_REDACT_SECRETS").ok();
    !matches!(val.as_deref(), Some("0" | "false" | "no" | "off"))
});

/// Known API key prefixes — match the prefix + contiguous token chars.
const PREFIX_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{10,}",           // OpenAI / OpenRouter / Anthropic
    r"ghp_[A-Za-z0-9]{10,}",            // GitHub PAT (classic)
    r"github_pat_[A-Za-z0-9_]{10,}",    // GitHub PAT (fine-grained)
    r"gho_[A-Za-z0-9]{10,}",            // GitHub OAuth access token
    r"ghu_[A-Za-z0-9]{10,}",            // GitHub user-to-server token
    r"ghs_[A-Za-z0-9]{10,}",            // GitHub server-to-server token
    r"ghr_[A-Za-z0-9]{10,}",            // GitHub refresh token
    r"xox[baprs]-[A-Za-z0-9-]{10,}",    // Slack tokens
    r"AIza[A-Za-z0-9_-]{30,}",          // Google API keys
    r"pplx-[A-Za-z0-9]{10,}",           // Perplexity
    r"fal_[A-Za-z0-9_-]{10,}",          // Fal.ai
    r"fc-[A-Za-z0-9]{10,}",             // Firecrawl
    r"bb_live_[A-Za-z0-9_-]{10,}",      // BrowserBase
    r"gAAAA[A-Za-z0-9_=-]{20,}",        // Codex encrypted tokens
    r"AKIA[A-Z0-9]{16}",                // AWS Access Key ID
    r"sk_live_[A-Za-z0-9]{10,}",        // Stripe secret key (live)
    r"sk_test_[A-Za-z0-9]{10,}",        // Stripe secret key (test)
    r"rk_live_[A-Za-z0-9]{10,}",        // Stripe restricted key
    r"SG\.[A-Za-z0-9_-]{10,}",          // SendGrid API key
    r"hf_[A-Za-z0-9]{10,}",             // HuggingFace token
    r"r8_[A-Za-z0-9]{10,}",             // Replicate API token
    r"npm_[A-Za-z0-9]{10,}",            // npm access token
    r"pypi-[A-Za-z0-9_-]{10,}",         // PyPI API token
    r"dop_v1_[A-Za-z0-9]{10,}",         // DigitalOcean PAT
    r"doo_v1_[A-Za-z0-9]{10,}",         // DigitalOcean OAuth
    r"am_[A-Za-z0-9_-]{10,}",           // AgentMail API key
    r"sk_[A-Za-z0-9_]{10,}",            // ElevenLabs TTS key (sk_ underscore)
    r"tvly-[A-Za-z0-9]{10,}",           // Tavily search API key
    r"exa_[A-Za-z0-9]{10,}",            // Exa search API key
    r"gsk_[A-Za-z0-9]{10,}",            // Groq Cloud API key
    r"syt_[A-Za-z0-9]{10,}",            // Matrix access token
    r"retaindb_[A-Za-z0-9]{10,}",       // RetainDB API key
    r"hsk-[A-Za-z0-9]{10,}",            // Hindsight API key
    r"mem0_[A-Za-z0-9]{10,}",           // Mem0 Platform API key
    r"brv_[A-Za-z0-9]{10,}",            // ByteRover API key
];

/// Compile known prefix patterns into one alternation.
/// Uses `\b` word boundary instead of lookbehind (unsupported by `regex` crate).
static PREFIX_RE: Lazy<Regex> = Lazy::new(|| {
    let alt = PREFIX_PATTERNS.join("|");
    Regex::new(&format!(r"\b({alt})\b")).unwrap()
});

/// ENV assignment patterns: KEY=value where KEY contains a secret-like name.
static ENV_ASSIGN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"([A-Z0-9_]{0,50}(?:API_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)[A-Z0-9_]{0,50})\s*=\s*(['"]?)(\S+)"#
    ).unwrap()
});

/// JSON field patterns: "apiKey": "value", "token": "value", etc.
static JSON_FIELD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)"(api_?key|token|secret|password|access_token|refresh_token|auth_token|bearer|secret_value|raw_secret|secret_input|key_material)"\s*:\s*"([^"]+)""#
    ).unwrap()
});

/// Authorization headers.
static AUTH_HEADER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(Authorization:\s*Bearer\s+)(\S+)").unwrap()
});

/// Telegram bot tokens.
static TELEGRAM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(bot)?(\d{8,}):([-A-Za-z0-9_]{30,})").unwrap()
});

/// Private key blocks.
static PRIVATE_KEY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY-----[\s\S]*?-----END[A-Z ]*PRIVATE KEY-----").unwrap()
});

/// Database connection strings.
static DB_CONNSTR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"((?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp)://[^:]+:)([^@]+)(@)").unwrap()
});

/// JWT tokens — header.payload[.signature], always start with "eyJ" (base64 for "{").
/// Mirrors Python `_JWT_RE` (commit ee9c0a3e).
static JWT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_=-]{4,}){0,2}").unwrap()
});

/// Discord user/role mentions: <@snowflake> or <@!snowflake>.
/// Snowflake IDs are 17–20 digit integers. Mirrors Python `_DISCORD_MENTION_RE` (commit ee9c0a3e).
static DISCORD_MENTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<@!?(\d{17,20})>").unwrap()
});

/// E.164 phone numbers — no lookahead (unsupported), post-filter handles boundary.
static SIGNAL_PHONE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\+[1-9]\d{6,14})").unwrap()
});

/// URL query params with sensitive values (access_token, code, api_key, etc.).
/// Mirrors Python _redact_url_query_params() (redact.py).
static URL_QUERY_PARAM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\b(?:https?|wss?)://[^?\s]*\?[^?\s]*)(?:\b(?:access_token|token|api_key|apikey|key|secret|password|code|auth|client_secret|refresh_token)=)([^&\s]+)"
    ).unwrap()
});

/// URL userinfo (user:password@host) redaction.
/// Mirrors Python _redact_url_userinfo() (redact.py).
static URL_USERINFO_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"((?:https?|wss?|ftp)://)([^:@/\s]+):([^@/\s]+)(@)").unwrap()
});

/// Mask a token, preserving prefix for long tokens.
fn mask_token(token: &str) -> String {
    if token.len() < 18 {
        "***".to_string()
    } else {
        format!("{}...{}", &token[..6], &token[token.len() - 4..])
    }
}

/// Apply all redaction patterns to a block of text.
///
/// Safe to call on any string — non-matching text passes through unchanged.
/// Disabled when security.redact_secrets is false in config.yaml.
pub fn redact_sensitive_text(text: &str) -> String {
    if text.is_empty() || !*REDACT_ENABLED {
        return text.to_string();
    }

    let mut result = text.to_string();

    // Known prefixes (sk-, ghp_, etc.)
    result = PREFIX_RE
        .replace_all(&result, |caps: &regex::Captures| mask_token(&caps[1]))
        .to_string();

    // ENV assignments: OPENAI_API_KEY=sk-abc...
    result = ENV_ASSIGN_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let name = &caps[1];
            let quote = &caps[2];
            let value = &caps[3];
            // Strip trailing quote from value if present
            let value = value.strip_suffix('\'').or_else(|| value.strip_suffix('"')).unwrap_or(value);
            format!("{}={quote}{}{quote}", name, mask_token(value))
        })
        .to_string();

    // JSON fields: "apiKey": "value"
    result = JSON_FIELD_RE
        .replace_all(&result, |caps: &regex::Captures| {
            format!("\"{}\": \"{}\"", &caps[1], mask_token(&caps[2]))
        })
        .to_string();

    // Authorization headers
    result = AUTH_HEADER_RE
        .replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}", &caps[1], mask_token(&caps[2]))
        })
        .to_string();

    // Telegram bot tokens
    result = TELEGRAM_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            format!("{}{}:***", prefix, &caps[2])
        })
        .to_string();

    // Private key blocks
    result = PRIVATE_KEY_RE
        .replace_all(&result, "[REDACTED PRIVATE KEY]")
        .to_string();

    // Database connection string passwords
    result = DB_CONNSTR_RE
        .replace_all(&result, |caps: &regex::Captures| {
            format!("{}***{}", &caps[1], &caps[3])
        })
        .to_string();

    // JWT tokens (eyJ... — base64-encoded JSON headers)
    result = JWT_RE
        .replace_all(&result, |caps: &regex::Captures| mask_token(&caps[0]))
        .to_string();

    // Discord user/role mentions (<@snowflake_id>)
    result = DISCORD_MENTION_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let has_bang = caps[0].chars().nth(2) == Some('!');
            if has_bang {
                "<@!***>".to_string()
            } else {
                "<@***>".to_string()
            }
        })
        .to_string();

    // E.164 phone numbers (Signal, WhatsApp)
    result = SIGNAL_PHONE_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let phone = &caps[1];
            if phone.len() <= 8 {
                format!("{}****{}", &phone[..2], &phone[phone.len() - 2..])
            } else {
                format!("{}****{}", &phone[..4], &phone[phone.len() - 4..])
            }
        })
        .to_string();

    // URL query params with sensitive values (access_token, key, etc.)
    result = URL_QUERY_PARAM_RE
        .replace_all(&result, |caps: &regex::Captures| {
            format!("{}***", &caps[1])
        })
        .to_string();

    // URL userinfo (user:password@host)
    result = URL_USERINFO_RE
        .replace_all(&result, |caps: &regex::Captures| {
            format!("{}{}:***{}", &caps[1], &caps[2], &caps[4])
        })
        .to_string();

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_token_short() {
        assert_eq!(mask_token("short"), "***");
        assert_eq!(mask_token("12345678901234567"), "***"); // 17 chars
    }

    #[test]
    fn test_mask_token_long() {
        let token = "abcdef1234567890xyzw"; // 20 chars
        let masked = mask_token(&token);
        assert_eq!(masked, "abcdef...xyzw");
    }

    #[test]
    fn test_redact_openai_key() {
        let text = "Using key sk-proj-abcdefghijklmnop1234567890";
        let result = redact_sensitive_text(text);
        assert!(result.contains("sk-pro"));
        assert!(result.contains("..."));
        assert!(!result.contains("sk-proj-abcdefghijklmnop1234567890"));
    }

    #[test]
    fn test_redact_env_assignment() {
        let text = "OPENAI_API_KEY=sk-abcdef1234567890abcd";
        let result = redact_sensitive_text(text);
        assert!(result.contains("OPENAI_API_KEY="));
        assert!(!result.contains("abcdef1234567890abcd"));
    }

    #[test]
    fn test_redact_json_field() {
        let text = r#"{"apiKey": "abcdef1234567890abcd"}"#;
        let result = redact_sensitive_text(text);
        assert!(result.contains(r#""apiKey": "abcdef...abcd""#));
    }

    #[test]
    fn test_redact_auth_header() {
        let text = "Authorization: Bearer abcdef1234567890abcd";
        let result = redact_sensitive_text(text);
        assert!(result.contains("Authorization: Bearer "));
        assert!(!result.contains("abcdef1234567890abcd"));
    }

    #[test]
    fn test_redact_private_key() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        let result = redact_sensitive_text(text);
        assert!(result.contains("[REDACTED PRIVATE KEY]"));
    }

    #[test]
    fn test_redact_db_connection_string() {
        let text = "postgresql://user:secret_password@localhost:5432/db";
        let result = redact_sensitive_text(text);
        assert!(result.contains("postgresql://user:***@"));
    }

    #[test]
    fn test_clean_content_passes() {
        let text = "Hello world, this is clean text";
        let result = redact_sensitive_text(text);
        assert_eq!(result, text);
    }

    #[test]
    fn test_redact_jwt_three_part() {
        // Full JWT: header.payload.signature
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let text = format!("auth={jwt}");
        let result = redact_sensitive_text(&text);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiIs"));
        assert!(result.contains("eyJhbG..."));
    }

    #[test]
    fn test_redact_jwt_two_part() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.dGVzdA";
        let text = format!("token {jwt}");
        let result = redact_sensitive_text(&text);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn test_redact_discord_mention() {
        let text = "Hello <@123456789012345678> and <@!987654321098765432>!";
        let result = redact_sensitive_text(text);
        assert!(result.contains("<@***>"));
        assert!(result.contains("<@!***>"));
        assert!(!result.contains("123456789012345678"));
    }

    #[test]
    fn test_redact_url_userinfo() {
        let text = "Connect to https://user:password123@example.com/api";
        let result = redact_sensitive_text(text);
        assert!(!result.contains("password123"));
    }
}

/// Redact sensitive data from a string for logging purposes.
/// Convenience wrapper around `redact_sensitive_text` that handles empty strings.
/// Mirrors Python RedactingFormatter pattern (agent/redact.py).
pub fn redact_for_log(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    redact_sensitive_text(text)
}
