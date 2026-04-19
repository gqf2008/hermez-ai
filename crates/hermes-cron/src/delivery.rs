//! Job result delivery — route output to platforms.
//!
//! Mirrors the Python `_deliver_result` and `_resolve_delivery_target` in `cron/scheduler.py`.

/// Delivery target resolved from a job's delivery config.
#[derive(Debug, Clone)]
pub enum DeliveryTarget {
    /// No delivery (local only).
    Local,
    /// Deliver back to the origin platform/chat.
    Origin {
        platform: String,
        chat_id: String,
        thread_id: Option<String>,
    },
    /// Deliver to a specific platform channel.
    Platform {
        platform: String,
        channel: String,
        thread_id: Option<String>,
    },
}

/// Known delivery platforms for validation.
const KNOWN_PLATFORMS: &[&str] = &[
    "telegram", "discord", "slack", "whatsapp", "signal",
    "matrix", "mattermost", "homeassistant", "dingtalk", "feishu",
    "wecom", "wecom_callback", "weixin", "sms", "email", "webhook", "bluebubbles",
];

/// Resolve the delivery target from a job's delivery config.
///
/// Supported formats:
/// - `"local"` → Local
/// - `"origin"` → Origin { platform, chat_id, thread_id } from job.origin
/// - `"platform:target"` → Platform { platform, channel: target }
/// - `"telegram"` → Platform { platform: "telegram", channel: TELEGRAM_HOME_CHANNEL }
pub fn resolve_delivery_target(deliver: &str, origin: Option<&serde_json::Value>) -> DeliveryTarget {
    match deliver {
        "local" | "" => DeliveryTarget::Local,
        "origin" => {
            if let Some(o) = origin {
                let platform = o.get("platform").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let chat_id = o.get("chat_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let thread_id = o.get("thread_id").and_then(|v| v.as_str()).map(String::from);
                if !platform.is_empty() && !chat_id.is_empty() {
                    return DeliveryTarget::Origin { platform, chat_id, thread_id };
                }
            }
            // Origin missing — try platform home channels as fallback
            for platform_name in &["matrix", "telegram", "discord", "slack", "bluebubbles"] {
                let env_key = format!("{}_HOME_CHANNEL", platform_name.to_uppercase());
                if let Ok(chat_id) = std::env::var(&env_key) {
                    if !chat_id.is_empty() {
                        return DeliveryTarget::Platform {
                            platform: platform_name.to_string(),
                            channel: chat_id,
                            thread_id: None,
                        };
                    }
                }
            }
            DeliveryTarget::Local
        }
        other => {
            if let Some((platform, rest)) = other.split_once(':') {
                DeliveryTarget::Platform {
                    platform: platform.to_string(),
                    channel: rest.to_string(),
                    thread_id: None,
                }
            } else {
                // Validate platform name
                let lower = other.to_lowercase();
                if !KNOWN_PLATFORMS.contains(&lower.as_str()) {
                    return DeliveryTarget::Local;
                }
                // Use HOME_CHANNEL env var
                let env_key = format!("{}_HOME_CHANNEL", other.to_uppercase());
                let channel = std::env::var(&env_key).unwrap_or_default();
                if channel.is_empty() {
                    return DeliveryTarget::Local;
                }
                DeliveryTarget::Platform {
                    platform: lower,
                    channel,
                    thread_id: None,
                }
            }
        }
    }
}

/// Deliver a job result to the target.
///
/// Returns None on success, or an error string on failure.
pub async fn deliver_result(target: &DeliveryTarget, job_name: &str, content: &str) -> Option<String> {
    match target {
        DeliveryTarget::Local => None,
        DeliveryTarget::Origin { platform, chat_id, thread_id } => {
            deliver_to_platform(platform, chat_id, thread_id.as_deref(), job_name, content).await
        }
        DeliveryTarget::Platform { platform, channel, thread_id } => {
            deliver_to_platform(platform, channel, thread_id.as_deref(), job_name, content).await
        }
    }
}

/// Deliver content to a platform via HTTP API.
async fn deliver_to_platform(
    platform: &str,
    channel: &str,
    thread_id: Option<&str>,
    job_name: &str,
    content: &str,
) -> Option<String> {
    // Optionally wrap the content with a header/footer (configurable via env)
    let wrap = std::env::var("HERMES_CRON_WRAP_RESPONSE")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    let message = if wrap {
        format!("Cronjob Response: {job_name}\n-------------\n\n{content}\n\nNote: The agent cannot see this message, and therefore cannot respond to it.")
    } else {
        content.to_string()
    };

    // Extract MEDIA: tags for native file attachments
    let (media_paths, text_content) = extract_media(&message);

    match platform {
        "telegram" => deliver_telegram(channel, &text_content, &media_paths, thread_id).await,
        "discord" => deliver_discord(channel, &text_content, &media_paths, thread_id).await,
        "slack" => deliver_slack(channel, &text_content, &media_paths, thread_id).await,
        "signal" => deliver_signal(channel, &text_content, &media_paths).await,
        "whatsapp" => deliver_whatsapp(channel, &text_content, &media_paths).await,
        "matrix" => deliver_matrix(channel, &text_content, &media_paths, thread_id).await,
        "mattermost" => deliver_mattermost(channel, &text_content, &media_paths, thread_id).await,
        "homeassistant" => deliver_homeassistant(channel, &text_content, &media_paths).await,
        "dingtalk" => deliver_dingtalk(channel, &text_content, &media_paths).await,
        "feishu" => deliver_feishu(channel, &text_content, &media_paths).await,
        "wecom" | "wecom_callback" => deliver_wecom(channel, &text_content, &media_paths).await,
        "weixin" => deliver_weixin(channel, &text_content, &media_paths).await,
        "bluebubbles" => deliver_bluebubbles(channel, &text_content, &media_paths).await,
        "sms" => deliver_sms(channel, &text_content).await,
        "email" => deliver_email(channel, job_name, &text_content).await,
        "webhook" => deliver_generic(platform, channel, &text_content).await,
        _ => deliver_generic(platform, channel, &text_content).await,
    }
}

/// Extract MEDIA: tags from content.
///
/// MEDIA: tags are lines like `MEDIA:/path/to/file.png` that indicate
/// native file attachments. Returns (media_paths, content_without_media).
fn extract_media(content: &str) -> (Vec<String>, String) {
    let mut media = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut text_lines = Vec::new();

    for line in &lines {
        if let Some(path) = line.strip_prefix("MEDIA:") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                media.push(trimmed.to_string());
            }
        } else {
            text_lines.push(*line);
        }
    }

    (media, text_lines.join("\n"))
}

/// Deliver to Telegram via Bot API.
async fn deliver_telegram(chat_id: &str, content: &str, media: &[String], _thread_id: Option<&str>) -> Option<String> {
    let bot_token = match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("TELEGRAM_BOT_TOKEN not set".to_string()),
    };

    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");

    // Send text first
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": content,
            "parse_mode": "Markdown",
        }))
        .send()
        .await;

    if let Err(e) = resp {
        return Some(format!("Telegram API error: {e}"));
    }

    // Send media as documents
    for path in media {
        let _ = send_telegram_document(&bot_token, chat_id, path).await;
    }

    None
}

async fn send_telegram_document(
    bot_token: &str,
    chat_id: &str,
    file_path: &str,
) -> Option<String> {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendDocument");

    let _file = std::fs::File::open(file_path).ok()?;
    let file_name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(
            "document",
            reqwest::multipart::Part::bytes(
                std::fs::read(file_path).ok()?,
            )
            .file_name(file_name),
        );

    let client = reqwest::Client::new();
    let resp = client.post(&url).multipart(form).send().await;

    if let Err(e) = resp {
        Some(format!("Telegram document error: {e}"))
    } else {
        None
    }
}

/// Deliver to Discord via webhook.
async fn deliver_discord(webhook_url: &str, content: &str, _media: &[String], _thread_id: Option<&str>) -> Option<String> {
    // Discord webhook URLs contain the token, so no separate auth needed
    if !webhook_url.starts_with("http") {
        return Some("Invalid Discord webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "content": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Discord webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to Slack via webhook.
async fn deliver_slack(webhook_url: &str, content: &str, _media: &[String], _thread_id: Option<&str>) -> Option<String> {
    if !webhook_url.starts_with("http") {
        return Some("Invalid Slack webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "text": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Slack webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to Signal via signal-cli REST API.
async fn deliver_signal(chat_id: &str, content: &str, _media: &[String]) -> Option<String> {
    let signal_url = std::env::var("SIGNAL_API_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());
    let signal_from = match std::env::var("SIGNAL_FROM") {
        Ok(f) => f,
        Err(_) => return Some("SIGNAL_FROM not set".to_string()),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{signal_url}/v2/send"))
        .json(&serde_json::json!({
            "message": content,
            "number": signal_from,
            "recipients": [chat_id],
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Signal API error: {e}"))
    } else {
        None
    }
}

/// Deliver to WhatsApp via Meta Cloud API.
async fn deliver_whatsapp(phone_number_id: &str, content: &str, _media: &[String]) -> Option<String> {
    let access_token = match std::env::var("WHATSAPP_ACCESS_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("WHATSAPP_ACCESS_TOKEN not set".to_string()),
    };

    let url = format!(
        "https://graph.facebook.com/v17.0/{phone_number_id}/messages"
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&access_token)
        .json(&serde_json::json!({
            "messaging_product": "whatsapp",
            "to": phone_number_id,
            "type": "text",
            "text": {"body": content},
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("WhatsApp API error: {e}"))
    } else {
        None
    }
}

/// Deliver to Matrix via Synapse Client-Server API.
async fn deliver_matrix(room_id: &str, content: &str, _media: &[String], _thread_id: Option<&str>) -> Option<String> {
    let homeserver = match std::env::var("MATRIX_HOMESERVER_URL") {
        Ok(u) => u,
        Err(_) => return Some("MATRIX_HOMESERVER_URL not set".to_string()),
    };
    let access_token = match std::env::var("MATRIX_ACCESS_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("MATRIX_ACCESS_TOKEN not set".to_string()),
    };

    let url = format!(
        "{homeserver}/_matrix/client/r0/rooms/{room_id}/send/m.room.message"
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&access_token)
        .json(&serde_json::json!({
            "msgtype": "m.text",
            "body": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Matrix API error: {e}"))
    } else {
        None
    }
}

/// Deliver to Mattermost via incoming webhook.
async fn deliver_mattermost(webhook_url: &str, content: &str, _media: &[String], _thread_id: Option<&str>) -> Option<String> {
    if !webhook_url.starts_with("http") {
        return Some("Invalid Mattermost webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "text": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Mattermost webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to Home Assistant via notify service.
async fn deliver_homeassistant(entity_id: &str, content: &str, _media: &[String]) -> Option<String> {
    let url = match std::env::var("HOMEASSISTANT_URL") {
        Ok(u) => u,
        Err(_) => "http://localhost:8123".to_string(),
    };
    let token = match std::env::var("HOMEASSISTANT_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("HOMEASSISTANT_TOKEN not set".to_string()),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/api/services/notify/send_message"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "entity_id": entity_id,
            "message": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Home Assistant API error: {e}"))
    } else {
        None
    }
}

/// Deliver to DingTalk via custom robot webhook.
async fn deliver_dingtalk(webhook_url: &str, content: &str, _media: &[String]) -> Option<String> {
    if !webhook_url.starts_with("http") {
        return Some("Invalid DingTalk webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "msgtype": "text",
            "text": { "content": content },
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("DingTalk webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to Feishu/Lark via custom robot webhook.
async fn deliver_feishu(webhook_url: &str, content: &str, _media: &[String]) -> Option<String> {
    if !webhook_url.starts_with("http") {
        return Some("Invalid Feishu webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "msg_type": "text",
            "content": { "text": content },
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Feishu webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to WeCom (Enterprise WeChat) via group robot webhook.
async fn deliver_wecom(webhook_url: &str, content: &str, _media: &[String]) -> Option<String> {
    if !webhook_url.starts_with("http") {
        return Some("Invalid WeCom webhook URL".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "msgtype": "text",
            "text": { "content": content },
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("WeCom webhook error: {e}"))
    } else {
        None
    }
}

/// Deliver to Weixin (WeChat) via platform webhook.
async fn deliver_weixin(chat_id: &str, content: &str, _media: &[String]) -> Option<String> {
    let url = match std::env::var("WEIXIN_API_URL") {
        Ok(u) => u,
        Err(_) => return Some("WEIXIN_API_URL not set".to_string()),
    };
    let token = match std::env::var("WEIXIN_ACCESS_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("WEIXIN_ACCESS_TOKEN not set".to_string()),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/message/send"))
        .query(&[("access_token", &token)])
        .json(&serde_json::json!({
            "touser": chat_id,
            "msgtype": "text",
            "text": { "content": content },
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Weixin API error: {e}"))
    } else {
        None
    }
}

/// Deliver to BlueBubbles via REST API.
async fn deliver_bluebubbles(chat_id: &str, content: &str, _media: &[String]) -> Option<String> {
    let url = match std::env::var("BLUEBUBBLES_URL") {
        Ok(u) => u,
        Err(_) => "http://localhost:1234".to_string(),
    };
    let password = match std::env::var("BLUEBUBBLES_PASSWORD") {
        Ok(p) => p,
        Err(_) => return Some("BLUEBUBBLES_PASSWORD not set".to_string()),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/api/message/text"))
        .query(&[("password", &password)])
        .json(&serde_json::json!({
            "chatGuid": chat_id,
            "message": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("BlueBubbles API error: {e}"))
    } else {
        None
    }
}

/// Deliver SMS via Twilio API.
async fn deliver_sms(phone_number: &str, content: &str) -> Option<String> {
    let account_sid = match std::env::var("TWILIO_ACCOUNT_SID") {
        Ok(s) => s,
        Err(_) => return Some("TWILIO_ACCOUNT_SID not set".to_string()),
    };
    let auth_token = match std::env::var("TWILIO_AUTH_TOKEN") {
        Ok(t) => t,
        Err(_) => return Some("TWILIO_AUTH_TOKEN not set".to_string()),
    };
    let from = match std::env::var("TWILIO_FROM") {
        Ok(f) => f,
        Err(_) => return Some("TWILIO_FROM not set".to_string()),
    };

    let url = format!("https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Messages.json");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .basic_auth(&account_sid, Some(&auth_token))
        .form(&serde_json::json!({
            "From": from,
            "To": phone_number,
            "Body": content,
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Twilio API error: {e}"))
    } else {
        None
    }
}

/// Deliver email via SMTP relay or API.
async fn deliver_email(to: &str, job_name: &str, content: &str) -> Option<String> {
    let from = match std::env::var("EMAIL_FROM") {
        Ok(f) => f,
        Err(_) => return Some("EMAIL_FROM not set".to_string()),
    };

    // Try SendGrid first
    if let Ok(api_key) = std::env::var("SENDGRID_API_KEY") {
        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.sendgrid.com/v3/mail/send")
            .bearer_auth(&api_key)
            .json(&serde_json::json!({
                "personalizations": [{"to": [{"email": to}]}],
                "from": {"email": from},
                "subject": format!("Cronjob Response: {job_name}"),
                "content": [{"type": "text/plain", "value": content}],
            }))
            .send()
            .await;
        return if let Err(e) = resp {
            Some(format!("SendGrid API error: {e}"))
        } else {
            None
        };
    }

    // Fallback: generic SMTP via Mailgun
    if let (Ok(domain), Ok(api_key)) = (std::env::var("MAILGUN_DOMAIN"), std::env::var("MAILGUN_API_KEY")) {
        let url = format!("https://api.mailgun.net/v3/{domain}/messages");
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .basic_auth("api", Some(&api_key))
            .form(&serde_json::json!({
                "from": from,
                "to": to,
                "subject": format!("Cronjob Response: {job_name}"),
                "text": content,
            }))
            .send()
            .await;
        return if let Err(e) = resp {
            Some(format!("Mailgun API error: {e}"))
        } else {
            None
        };
    }

    Some("No email provider configured (SENDGRID_API_KEY or MAILGUN_*)".to_string())
}

/// Generic HTTP POST delivery.
async fn deliver_generic(platform: &str, channel: &str, content: &str) -> Option<String> {
    // Try common webhook patterns
    let webhook_var = format!("{platform}_WEBHOOK_URL");
    let url = match std::env::var(&webhook_var) {
        Ok(u) => u,
        Err(_) => return Some(format!("{webhook_var} not set")),
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "channel": channel,
            "text": content,
            "source": "hermes-cron",
        }))
        .send()
        .await;

    if let Err(e) = resp {
        Some(format!("Generic webhook error: {e}"))
    } else {
        None
    }
}

/// Check if a response contains the SILENT marker (case-insensitive).
pub fn is_silent(content: &str) -> bool {
    content.trim().to_uppercase().contains("[SILENT]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_local() {
        let target = resolve_delivery_target("local", None);
        assert!(matches!(target, DeliveryTarget::Local));
    }

    #[test]
    fn test_resolve_origin() {
        let origin = serde_json::json!({
            "platform": "telegram",
            "chat_id": "12345"
        });
        let target = resolve_delivery_target("origin", Some(&origin));
        match target {
            DeliveryTarget::Origin { platform, chat_id, .. } => {
                assert_eq!(platform, "telegram");
                assert_eq!(chat_id, "12345");
            }
            _ => panic!("Expected Origin, got {target:?}"),
        }
    }

    #[test]
    fn test_resolve_platform_colon() {
        let target = resolve_delivery_target("telegram:67890", None);
        match target {
            DeliveryTarget::Platform { platform, channel, .. } => {
                assert_eq!(platform, "telegram");
                assert_eq!(channel, "67890");
            }
            _ => panic!("Expected Platform, got {target:?}"),
        }
    }

    #[test]
    fn test_resolve_platform_env() {
        std::env::set_var("SLACK_HOME_CHANNEL", "#cron-alerts");
        let target = resolve_delivery_target("slack", None);
        match target {
            DeliveryTarget::Platform { platform, channel, .. } => {
                assert_eq!(platform, "slack");
                assert_eq!(channel, "#cron-alerts");
            }
            _ => panic!("Expected Platform, got {target:?}"),
        }
        std::env::remove_var("SLACK_HOME_CHANNEL");
    }

    #[test]
    fn test_extract_media() {
        let content = "Here is the result.\nMEDIA:/tmp/chart.png\nMEDIA:/tmp/report.pdf\nDone.";
        let (media, text) = extract_media(content);
        assert_eq!(media, vec!["/tmp/chart.png", "/tmp/report.pdf"]);
        assert_eq!(text, "Here is the result.\nDone.");
    }

    #[test]
    fn test_is_silent() {
        assert!(is_silent("[SILENT] No action needed"));
        assert!(!is_silent("Regular output"));
    }

    #[tokio::test]
    async fn test_deliver_local() {
        let target = DeliveryTarget::Local;
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_deliver_matrix_requires_env() {
        let target = resolve_delivery_target("matrix:room123", None);
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_some());
        let err = result.unwrap();
        assert!(err.contains("MATRIX_ACCESS_TOKEN") || err.contains("MATRIX_HOMESERVER_URL"));
    }

    #[tokio::test]
    async fn test_deliver_homeassistant_requires_token() {
        let target = resolve_delivery_target("homeassistant:notify.user", None);
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("HOMEASSISTANT_TOKEN"));
    }

    #[tokio::test]
    async fn test_deliver_weixin_requires_env() {
        let target = resolve_delivery_target("weixin:user123", None);
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_some());
        let err = result.unwrap();
        assert!(err.contains("WEIXIN_API_URL") || err.contains("WEIXIN_ACCESS_TOKEN"));
    }

    #[tokio::test]
    async fn test_deliver_bluebubbles_requires_password() {
        let target = resolve_delivery_target("bluebubbles:chat-abc", None);
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("BLUEBUBBLES_PASSWORD"));
    }

    #[tokio::test]
    async fn test_deliver_generic_requires_webhook_url() {
        let target = resolve_delivery_target("unknown_platform:channel", None);
        let result = deliver_result(&target, "test", "output").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("not set"));
    }
}
