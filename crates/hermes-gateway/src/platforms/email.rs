//! Email platform adapter.
//!
//! Mirrors Python `gateway/platforms/email.py`.
//!
//! Uses blocking IMAP (via the `imap` crate) inside `tokio::task::spawn_blocking`
//! to poll for incoming messages, and blocking SMTP (via `lettre`) to send
//! replies.  MIME parsing is handled by `mailparse`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use lettre::message::{header::ContentType, Attachment, Mailbox, Message, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{SmtpTransport, Transport};
use mailparse::{parse_mail, ParsedMail};
use parking_lot::Mutex;
use regex::Regex;
use tracing::{debug, info, warn};

/// Max length for the text body we forward to the agent.
const MAX_MESSAGE_LENGTH: usize = 50_000;
/// Cap for the UID dedup set.
const SEEN_UIDS_MAX: usize = 2000;
/// Default poll interval in seconds.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 15;
/// IMAP operation timeout (used only for logging / retries – the `imap`
/// crate does not expose a configurable timeout).
const _IMAP_TIMEOUT_SECS: u64 = 30;

/// Image extensions treated as inline images.
const IMAGE_EXTS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp"];

/// Patterns that indicate an automated / no-reply sender.
const NOREPLY_PATTERNS: &[&str] = &[
    "noreply",
    "no-reply",
    "no_reply",
    "donotreply",
    "do-not-reply",
    "mailer-daemon",
    "postmaster",
    "bounce",
    "notifications@",
    "automated@",
    "auto-confirm",
    "auto-reply",
    "automailer",
];

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Email platform configuration.
#[derive(Debug, Clone)]
pub struct EmailConfig {
    /// Email address used by the agent.
    pub address: String,
    /// Password or app-specific password.
    pub password: String,
    /// IMAP server host.
    pub imap_host: String,
    /// IMAP server port (default 993).
    pub imap_port: u16,
    /// SMTP server host.
    pub smtp_host: String,
    /// SMTP server port (default 587).
    pub smtp_port: u16,
    /// Seconds between mailbox checks.
    pub poll_interval_secs: u64,
    /// Ignore all attachment/inline parts.
    pub skip_attachments: bool,
    /// If non-empty, only accept mail from these addresses.
    pub allowed_users: Vec<String>,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            password: String::new(),
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 587,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            skip_attachments: false,
            allowed_users: Vec::new(),
        }
    }
}

impl EmailConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let address = std::env::var("EMAIL_ADDRESS").unwrap_or_default();
        let password = std::env::var("EMAIL_PASSWORD").unwrap_or_default();
        let imap_host = std::env::var("EMAIL_IMAP_HOST").unwrap_or_default();
        let imap_port = std::env::var("EMAIL_IMAP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(993);
        let smtp_host = std::env::var("EMAIL_SMTP_HOST").unwrap_or_default();
        let smtp_port = std::env::var("EMAIL_SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(587);
        let poll_interval_secs = std::env::var("EMAIL_POLL_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
        let skip_attachments = std::env::var("EMAIL_SKIP_ATTACHMENTS")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);
        let allowed_users = std::env::var("EMAIL_ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            address,
            password,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            poll_interval_secs,
            skip_attachments,
            allowed_users,
        }
    }

    /// Whether the minimum required fields are populated.
    pub fn is_configured(&self) -> bool {
        !self.address.is_empty()
            && !self.password.is_empty()
            && !self.imap_host.is_empty()
            && !self.smtp_host.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

/// Inbound message event from Email.
#[derive(Debug, Clone)]
pub struct EmailMessageEvent {
    /// Original Message-ID header.
    pub message_id: String,
    /// Sender email address (used as chat_id).
    pub chat_id: String,
    /// Sender display name.
    pub sender_name: String,
    /// Subject line.
    pub subject: String,
    /// Normalised text content.
    pub content: String,
    /// In-Reply-To header (if present).
    pub in_reply_to: Option<String>,
    /// Local paths for cached attachments.
    pub media_paths: Vec<String>,
    /// MIME types for attachments.
    pub media_types: Vec<String>,
    /// `text`, `image`, or `document`.
    pub msg_type: String,
}

// ---------------------------------------------------------------------------
// Thread context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct ThreadContext {
    subject: String,
    message_id: String,
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Email platform adapter.
pub struct EmailAdapter {
    pub config: EmailConfig,
    /// UID dedup set.
    seen_uids: Mutex<HashSet<u32>>,
    /// Per-sender thread context.
    thread_context: Mutex<HashMap<String, ThreadContext>>,
    /// Connection flag.
    connected: AtomicBool,
}

impl EmailAdapter {
    /// Return the poll interval configured for this adapter.
    pub fn poll_interval_secs(&self) -> u64 {
        self.config.poll_interval_secs
    }

    pub fn new(config: EmailConfig) -> Self {
        Self {
            config,
            seen_uids: Mutex::new(HashSet::new()),
            thread_context: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(false),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.config.is_configured()
    }

    /// Test IMAP and SMTP connectivity and seed the UID set with existing
    /// messages so we only process new mail.
    pub async fn connect(&self) -> Result<(), String> {
        let config = self.config.clone();
        let uids = tokio::task::spawn_blocking(move || seed_seen_uids(&config))
            .await
            .map_err(|e| format!("IMAP seed task panicked: {e}"))?
            .map_err(|e| format!("IMAP seed failed: {e}"))?;

        {
            let mut seen = self.seen_uids.lock();
            for uid in uids {
                seen.insert(uid);
            }
            trim_seen_uids(&mut seen);
        }

        let config = self.config.clone();
        tokio::task::spawn_blocking(move || test_smtp(&config))
            .await
            .map_err(|e| format!("SMTP test task panicked: {e}"))?
            .map_err(|e| format!("SMTP test failed: {e}"))?;

        self.connected.store(true, Ordering::SeqCst);
        info!("[Email] Connected as {}", self.config.address);
        Ok(())
    }

    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        info!("[Email] Disconnected.");
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Fetch any new (unseen) messages from the INBOX.
    pub async fn get_updates(&self) -> Result<Vec<EmailMessageEvent>, String> {
        if !self.is_connected() {
            return Err("Not connected".into());
        }

        let config = self.config.clone();
        let mut seen = self.seen_uids.lock().clone();

        let (events, new_uids) = tokio::task::spawn_blocking(move || {
            fetch_new_messages(&config, &mut seen)
        })
        .await
        .map_err(|e| format!("IMAP fetch task panicked: {e}"))?
        .map_err(|e| format!("IMAP fetch failed: {e}"))?;

        {
            let mut guard = self.seen_uids.lock();
            for uid in new_uids {
                guard.insert(uid);
            }
            trim_seen_uids(&mut guard);
        }

        // Update thread context so replies can thread correctly.
        {
            let mut ctx = self.thread_context.lock();
            for event in &events {
                ctx.insert(
                    event.chat_id.clone(),
                    ThreadContext {
                        subject: event.subject.clone(),
                        message_id: event.message_id.clone(),
                    },
                );
            }
        }

        Ok(events)
    }

    /// Send a plain-text reply.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        self.send_email(chat_id, text, None, None, None).await
    }

    /// Send a file as an attachment.
    pub async fn send_document(
        &self,
        chat_id: &str,
        file_path: &str,
        file_name: Option<&str>,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_email(chat_id, caption.unwrap_or(""), Some(file_path), file_name, None)
            .await
    }

    /// Send a file from an in-memory byte slice.
    pub async fn send_document_bytes(
        &self,
        chat_id: &str,
        data: &[u8],
        file_name: &str,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_email(chat_id, caption.unwrap_or(""), None, Some(file_name), Some(data.to_vec()))
            .await
    }

    /// Send an image file as an attachment.
    pub async fn send_image(
        &self,
        chat_id: &str,
        file_path: &str,
        file_name: Option<&str>,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_document(chat_id, file_path, file_name, caption).await
    }

    /// Send an image from an in-memory byte slice.
    pub async fn send_image_bytes(
        &self,
        chat_id: &str,
        data: &[u8],
        file_name: &str,
        caption: Option<&str>,
    ) -> Result<String, String> {
        self.send_document_bytes(chat_id, data, file_name, caption).await
    }

    // -----------------------------------------------------------------
    // Internal send helper
    // -----------------------------------------------------------------

    async fn send_email(
        &self,
        to_addr: &str,
        body: &str,
        attachment_path: Option<&str>,
        attachment_name: Option<&str>,
        attachment_bytes: Option<Vec<u8>>,
    ) -> Result<String, String> {
        let config = self.config.clone();
        let to_addr = to_addr.to_string();
        let body = body.to_string();
        let attachment_path = attachment_path.map(|s| s.to_string());
        let attachment_name = attachment_name.map(|s| s.to_string());
        let ctx = self.thread_context.lock().get(&to_addr).cloned();

        tokio::task::spawn_blocking(move || {
            send_email_blocking(
                &config,
                &to_addr,
                &body,
                attachment_path.as_deref(),
                attachment_name.as_deref(),
                attachment_bytes,
                ctx.as_ref(),
            )
        })
        .await
        .map_err(|e| format!("SMTP send task panicked: {e}"))?
    }
}

// ---------------------------------------------------------------------------
// Blocking IMAP helpers
// ---------------------------------------------------------------------------

fn seed_seen_uids(config: &EmailConfig) -> Result<Vec<u32>, String> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("TLS build failed: {e}"))?;
    let client = imap::connect(
        (&config.imap_host as &str, config.imap_port),
        &config.imap_host,
        &tls,
    )
    .map_err(|e| format!("IMAP connect failed: {e}"))?;

    let mut imap_session = client
        .login(&config.address, &config.password)
        .map_err(|(err, _client)| format!("IMAP login failed: {err}"))?;

    imap_session
        .select("INBOX")
        .map_err(|e| format!("IMAP select failed: {e}"))?;

    let uids = match imap_session.uid_search("ALL") {
        Ok(set) => set.into_iter().collect::<Vec<u32>>(),
        Err(e) => {
            warn!("IMAP search failed: {e}");
            Vec::new()
        }
    };

    let _ = imap_session.logout();
    Ok(uids)
}

fn test_smtp(config: &EmailConfig) -> Result<(), String> {
    let creds = Credentials::new(config.address.clone(), config.password.clone());
    let mailer = SmtpTransport::relay(&config.smtp_host)
        .map_err(|e| format!("SMTP relay setup failed: {e}"))?
        .credentials(creds)
        .port(config.smtp_port)
        .build();

    if !mailer
        .test_connection()
        .map_err(|e| format!("SMTP test_connection failed: {e}"))?
    {
        return Err("SMTP server did not accept the connection".into());
    }
    Ok(())
}

fn fetch_new_messages(
    config: &EmailConfig,
    seen: &mut HashSet<u32>,
) -> Result<(Vec<EmailMessageEvent>, Vec<u32>), String> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("TLS build failed: {e}"))?;
    let client = imap::connect(
        (&config.imap_host as &str, config.imap_port),
        &config.imap_host,
        &tls,
    )
    .map_err(|e| format!("IMAP connect failed: {e}"))?;

    let mut imap_session = client
        .login(&config.address, &config.password)
        .map_err(|(err, _client)| format!("IMAP login failed: {err}"))?;

    imap_session
        .select("INBOX")
        .map_err(|e| format!("IMAP select failed: {e}"))?;

    let uids = match imap_session.uid_search("UNSEEN") {
        Ok(set) => set.into_iter().collect::<Vec<u32>>(),
        Err(e) => {
            warn!("IMAP UNSEEN search failed: {e}");
            let _ = imap_session.logout();
            return Ok((Vec::new(), Vec::new()));
        }
    };

    let mut events = Vec::new();
    let mut new_uids = Vec::new();

    for uid in uids {
        if seen.contains(&uid) {
            continue;
        }
        seen.insert(uid);
        new_uids.push(uid);

        let messages = match imap_session.uid_fetch(format!("{uid}"), "RFC822") {
            Ok(ms) => ms,
            Err(e) => {
                warn!("IMAP fetch failed for UID {uid}: {e}");
                continue;
            }
        };

        for msg in &messages {
            let body = match msg.body() {
                Some(b) => b,
                None => continue,
            };

            let parsed = match parse_mail(body) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to parse email UID {uid}: {e}");
                    continue;
                }
            };

            let headers = extract_headers(&parsed);

            let from = headers.get("from").cloned().unwrap_or_default();
            let sender_addr = extract_email_address(&from);
            let sender_name = extract_sender_name(&from);

            // Skip self-messages.
            if sender_addr == config.address.to_lowercase() {
                continue;
            }

            // Skip automated senders.
            if is_automated_sender(&sender_addr, &headers) {
                debug!("[Email] Skipping automated sender: {sender_addr}");
                continue;
            }

            // Allowed-users filter.
            if !config.allowed_users.is_empty() && !config.allowed_users.contains(&sender_addr) {
                debug!("[Email] Sender {sender_addr} not in allowed list");
                continue;
            }

            let subject = decode_rfc2047(&headers.get("subject").cloned().unwrap_or_default());
            let message_id = headers.get("message-id").cloned().unwrap_or_default();
            let in_reply_to = headers.get("in-reply-to").cloned();

            let text_body = extract_text_body(&parsed);
            let attachments = if config.skip_attachments {
                Vec::new()
            } else {
                extract_attachments(&parsed)
            };

            let mut media_paths = Vec::new();
            let mut media_types = Vec::new();
            let mut msg_type = "text".to_string();

            for att in &attachments {
                media_paths.push(att.path.clone());
                media_types.push(att.media_type.clone());
                if att.att_type == "image" {
                    msg_type = "image".to_string();
                } else if msg_type == "text" {
                    msg_type = "document".to_string();
                }
            }

            let mut content = text_body;
            if content.len() > MAX_MESSAGE_LENGTH {
                content.truncate(MAX_MESSAGE_LENGTH);
                content.push_str("\n\n[message truncated]");
            }

            if !subject.is_empty() && !subject.to_lowercase().starts_with("re:") {
                content = format!("[Subject: {subject}]\n\n{content}");
            }

            // Append media references so the agent sees them.
            for (path, mime) in media_paths.iter().zip(media_types.iter()) {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&format!("[{mime}: {path}]"));
            }

            events.push(EmailMessageEvent {
                message_id: message_id.clone(),
                chat_id: sender_addr.clone(),
                sender_name: sender_name.clone(),
                subject: subject.clone(),
                content: content.trim().to_string(),
                in_reply_to: in_reply_to.clone(),
                media_paths,
                media_types,
                msg_type,
            });
        }
    }

    let _ = imap_session.logout();
    Ok((events, new_uids))
}

fn trim_seen_uids(seen: &mut HashSet<u32>) {
    if seen.len() <= SEEN_UIDS_MAX {
        return;
    }
    let mut sorted: Vec<u32> = seen.iter().copied().collect();
    sorted.sort_unstable();
    let keep = SEEN_UIDS_MAX / 2;
    let to_keep: HashSet<u32> = sorted.into_iter().rev().take(keep).collect();
    *seen = to_keep;
    debug!("[Email] Trimmed seen UIDs to {} entries", seen.len());
}

// ---------------------------------------------------------------------------
// Blocking SMTP helper
// ---------------------------------------------------------------------------

fn send_email_blocking(
    config: &EmailConfig,
    to_addr: &str,
    body: &str,
    attachment_path: Option<&str>,
    attachment_name: Option<&str>,
    attachment_bytes: Option<Vec<u8>>,
    ctx: Option<&ThreadContext>,
) -> Result<String, String> {
    let from_mb: Mailbox = config
        .address
        .parse()
        .map_err(|e| format!("Invalid from address: {e}"))?;
    let to_mb: Mailbox = to_addr
        .parse()
        .map_err(|e| format!("Invalid to address: {e}"))?;

    let subject = ctx
        .map(|c| c.subject.clone())
        .unwrap_or_else(|| "Hermes Agent".to_string());
    let subject = if subject.to_lowercase().starts_with("re:") {
        subject
    } else {
        format!("Re: {subject}")
    };

    let mut builder = Message::builder()
        .from(from_mb.clone())
        .to(to_mb)
        .subject(subject);

    // Threading headers.
    let original_msg_id = ctx.map(|c| c.message_id.clone()).unwrap_or_default();
    if !original_msg_id.is_empty() {
        builder = builder.header(lettre::message::header::InReplyTo::from(
            original_msg_id.clone(),
        ));
        builder = builder.header(lettre::message::header::References::from(
            original_msg_id,
        ));
    }

    let msg_id = format!(
        "<hermes-{}@{}>",
        uuid::Uuid::new_v4().simple(),
        from_mb.email.domain()
    );
    builder = builder.message_id(Some(msg_id.clone()));

    // Build body / attachments.
    let email = if attachment_bytes.is_some() || attachment_path.is_some() {
        let data = if let Some(bytes) = attachment_bytes {
            bytes
        } else if let Some(path) = attachment_path {
            std::fs::read(path)
                .map_err(|e| format!("Failed to read attachment {path}: {e}"))?
        } else {
            vec![]
        };
        let filename = attachment_name
            .map(|s| s.to_string())
            .or_else(|| attachment_path.and_then(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().to_string())))
            .unwrap_or_else(|| "attachment".to_string());
        let ct = guess_mime_type(&filename);

        let attachment = Attachment::new(filename).body(data, ct);

        if body.is_empty() {
            builder
                .multipart(lettre::message::MultiPart::mixed().singlepart(attachment))
                .map_err(|e| format!("Failed to build email: {e}"))?
        } else {
            let plain = SinglePart::builder()
                .header(ContentType::parse("text/plain").unwrap_or_else(|_| {
                    ContentType::parse("text/plain").expect("static mime is valid")
                }))
                .body(body.to_string());
            builder
                .multipart(
                    lettre::message::MultiPart::mixed()
                        .singlepart(plain)
                        .singlepart(attachment),
                )
                .map_err(|e| format!("Failed to build multipart email: {e}"))?
        }
    } else {
        builder
            .header(ContentType::parse("text/plain").unwrap_or_else(|_| {
                    ContentType::parse("text/plain").expect("static mime is valid")
                }))
            .body(body.to_string())
            .map_err(|e| format!("Failed to build email: {e}"))?
    };

    let creds = Credentials::new(config.address.clone(), config.password.clone());
    let mailer = SmtpTransport::relay(&config.smtp_host)
        .map_err(|e| format!("SMTP relay setup failed: {e}"))?
        .credentials(creds)
        .port(config.smtp_port)
        .build();

    mailer
        .send(&email)
        .map_err(|e| format!("SMTP send failed: {e}"))?;

    info!("[Email] Sent reply to {to_addr}");
    Ok(msg_id)
}

// ---------------------------------------------------------------------------
// MIME helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AttachmentInfo {
    path: String,
    filename: String,
    att_type: String,
    media_type: String,
}

fn extract_attachments(parsed: &ParsedMail) -> Vec<AttachmentInfo> {
    let mut results = Vec::new();
    for part in &parsed.subparts {
        let disp = part.get_content_disposition();
        let is_attachment = disp.disposition == mailparse::DispositionType::Attachment
            || disp.disposition == mailparse::DispositionType::Inline;
        if !is_attachment {
            continue;
        }

        let ct = part.ctype.mimetype.clone();
        // Skip plain text / html body parts unless they are explicit attachments.
        if (ct == "text/plain" || ct == "text/html")
            && disp.disposition != mailparse::DispositionType::Attachment
        {
            continue;
        }

        let filename = disp
            .params
            .get("filename")
            .or_else(|| disp.params.get("name"))
            .cloned()
            .unwrap_or_else(|| {
                let ext = part.ctype.mimetype.split('/').nth(1).unwrap_or("bin");
                format!("attachment.{ext}")
            });

        let data = match part.get_body_raw() {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to decode attachment body: {e}");
                continue;
            }
        };

        let ext = Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let (path, att_type) = if IMAGE_EXTS.iter().any(|e| ext == *e) {
            match cache_image_from_bytes(&data, &ext) {
                Ok(p) => (p, "image".to_string()),
                Err(e) => {
                    warn!("Skipping non-image attachment {filename}: {e}");
                    continue;
                }
            }
        } else {
            match cache_document_from_bytes(&data, &filename) {
                Ok(p) => (p, "document".to_string()),
                Err(e) => {
                    warn!("Failed to cache document {filename}: {e}");
                    continue;
                }
            }
        };

        results.push(AttachmentInfo {
            path,
            filename,
            att_type,
            media_type: ct,
        });
    }
    results
}

fn extract_text_body(parsed: &ParsedMail) -> String {
    // Try text/plain first.
    for part in &parsed.subparts {
        if part.ctype.mimetype == "text/plain" {
            if let Ok(text) = part.get_body() {
                return text;
            }
        }
    }
    // Fallback to text/html.
    for part in &parsed.subparts {
        if part.ctype.mimetype == "text/html" {
            if let Ok(html) = part.get_body() {
                return strip_html(&html);
            }
        }
    }
    // Non-multipart fallback.
    if parsed.subparts.is_empty() {
        if parsed.ctype.mimetype == "text/plain" {
            if let Ok(text) = parsed.get_body() {
                return text;
            }
        } else if parsed.ctype.mimetype == "text/html" {
            if let Ok(html) = parsed.get_body() {
                return strip_html(&html);
            }
        }
    }
    String::new()
}

fn strip_html(html: &str) -> String {
    static BR_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static P_OPEN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static P_CLOSE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static TAG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static MULTI_NL_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let br_re = BR_RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
    let p_open_re = P_OPEN_RE.get_or_init(|| Regex::new(r"(?i)<p[^>]*>").unwrap());
    let p_close_re = P_CLOSE_RE.get_or_init(|| Regex::new(r"(?i)</p>").unwrap());
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    let multi_nl_re = MULTI_NL_RE.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let mut text = html.to_string();
    text = br_re.replace_all(&text, "\n").into_owned();
    text = p_open_re.replace_all(&text, "\n").into_owned();
    text = p_close_re.replace_all(&text, "\n").into_owned();
    text = tag_re.replace_all(&text, "").into_owned();
    text = text.replace("&nbsp;", " ");
    text = text.replace("&amp;", "&");
    text = text.replace("&lt;", "<");
    text = text.replace("&gt;", ">");
    text = multi_nl_re.replace_all(&text, "\n\n").into_owned();
    text.trim().to_string()
}

fn extract_headers(parsed: &ParsedMail) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for header in &parsed.headers {
        let key = header.get_key().to_lowercase();
        let value = header.get_value();
        map.insert(key, value);
    }
    map
}

fn extract_email_address(raw: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<([^>]+)>").unwrap());
    if let Some(cap) = re.captures(raw) {
        cap.get(1)
            .map(|m| m.as_str().trim().to_lowercase())
            .unwrap_or_default()
    } else {
        raw.trim().to_lowercase()
    }
}

fn extract_sender_name(raw: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    let name = re.replace_all(raw, "").trim().to_string();
    name.trim_matches('"').to_string()
}

fn decode_rfc2047(raw: &str) -> String {
    // mailparse already decodes RFC-2047 in `get_value()`, but this helper
    // is kept for clarity when dealing with raw strings.
    raw.to_string()
}

fn is_automated_sender(address: &str, headers: &HashMap<String, String>) -> bool {
    let addr = address.to_lowercase();
    if NOREPLY_PATTERNS.iter().any(|p| addr.contains(p)) {
        return true;
    }
    if let Some(val) = headers.get("auto-submitted") {
        if val.trim().to_lowercase() != "no" {
            return true;
        }
    }
    if let Some(val) = headers.get("precedence") {
        let v = val.trim().to_lowercase();
        if v == "bulk" || v == "list" || v == "junk" {
            return true;
        }
    }
    if headers.contains_key("x-auto-response-suppress") {
        return true;
    }
    if headers.contains_key("list-unsubscribe") {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Cache helpers (mirrors wecom.rs)
// ---------------------------------------------------------------------------

fn cache_image_from_bytes(data: &[u8], ext: &str) -> Result<String, String> {
    if !looks_like_image(data) {
        return Err("Data does not look like an image".into());
    }
    let cache_dir = hermes_core::get_hermes_home().join("cache").join("images");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create image cache dir: {e}"))?;
    let name = format!("img_{}{ext}", uuid::Uuid::new_v4().simple());
    let path = cache_dir.join(&name);
    std::fs::write(&path, data).map_err(|e| format!("Failed to write image cache: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn cache_document_from_bytes(data: &[u8], filename: &str) -> Result<String, String> {
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document")
        .replace('\x00', "")
        .trim()
        .to_string();
    if safe_name.is_empty() {
        return cache_image_from_bytes(data, ".bin");
    }
    let cache_dir = hermes_core::get_hermes_home().join("cache").join("documents");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create document cache dir: {e}"))?;
    let name = format!("doc_{}_{safe_name}", uuid::Uuid::new_v4().simple());
    let path = cache_dir.join(&name);
    std::fs::write(&path, data).map_err(|e| format!("Failed to write document cache: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn looks_like_image(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return true;
    }
    if data.starts_with(b"\xff\xd8\xff") {
        return true;
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return true;
    }
    if data.starts_with(b"BM") {
        return true;
    }
    if data.starts_with(b"RIFF") && data.len() > 8 && &data[8..12] == b"WEBP" {
        return true;
    }
    false
}

fn guess_mime_type(filename: &str) -> ContentType {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    };
    ContentType::parse(mime).unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = EmailConfig::default();
        assert!(config.address.is_empty());
        assert!(config.password.is_empty());
        assert_eq!(config.imap_port, 993);
        assert_eq!(config.smtp_port, 587);
        assert_eq!(config.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert!(!config.skip_attachments);
        assert!(config.allowed_users.is_empty());
    }

    #[test]
    fn test_config_is_configured() {
        let mut config = EmailConfig::default();
        assert!(!config.is_configured());
        config.address = "test@example.com".to_string();
        assert!(!config.is_configured());
        config.password = "pass".to_string();
        assert!(!config.is_configured());
        config.imap_host = "imap.example.com".to_string();
        assert!(!config.is_configured());
        config.smtp_host = "smtp.example.com".to_string();
        assert!(config.is_configured());
    }

    #[test]
    fn test_extract_email_address() {
        assert_eq!(
            extract_email_address("John Doe <john@example.com>"),
            "john@example.com"
        );
        assert_eq!(
            extract_email_address("<alice@domain.org>"),
            "alice@domain.org"
        );
        assert_eq!(
            extract_email_address("bare@address.com"),
            "bare@address.com"
        );
        assert_eq!(extract_email_address(""), "");
    }

    #[test]
    fn test_extract_sender_name() {
        assert_eq!(
            extract_sender_name("\"John Doe\" <john@example.com>"),
            "John Doe"
        );
        assert_eq!(
            extract_sender_name("Jane Smith <jane@example.com>"),
            "Jane Smith"
        );
        assert_eq!(
            extract_sender_name("<bare@example.com>"),
            ""
        );
    }

    #[test]
    fn test_is_automated_sender() {
        let mut headers = HashMap::new();
        assert!(is_automated_sender("noreply@example.com", &headers));
        assert!(is_automated_sender("no-reply@company.com", &headers));
        assert!(is_automated_sender("mailer-daemon@bounce.host", &headers));
        assert!(!is_automated_sender("user@example.com", &headers));

        headers.insert("auto-submitted".to_string(), "yes".to_string());
        assert!(is_automated_sender("user@example.com", &headers));

        headers.remove("auto-submitted");
        headers.insert("auto-submitted".to_string(), "no".to_string());
        assert!(!is_automated_sender("user@example.com", &headers));

        headers.remove("auto-submitted");
        headers.insert("precedence".to_string(), "bulk".to_string());
        assert!(is_automated_sender("news@list.com", &headers));

        headers.remove("precedence");
        headers.insert("list-unsubscribe".to_string(), "<https://x.com>".to_string());
        assert!(is_automated_sender("news@list.com", &headers));
    }

    #[test]
    fn test_strip_html() {
        assert_eq!(
            strip_html("<p>Hello</p><br/><b>World</b>"),
            "Hello\n\nWorld"
        );
        assert_eq!(
            strip_html("A &amp; B &lt; C &gt; D &nbsp; E"),
            "A & B < C > D   E"
        );
        assert_eq!(
            strip_html("<a href='x'>link</a>"),
            "link"
        );
    }

    #[test]
    fn test_extract_text_body() {
        let raw = "From: test@test.com\r\n\
                   Content-Type: text/plain\r\n\r\nHello world";
        let parsed = parse_mail(raw.as_bytes()).unwrap();
        assert_eq!(extract_text_body(&parsed), "Hello world");
    }

    #[test]
    fn test_extract_text_body_html_fallback() {
        let raw = "From: test@test.com\r\n\
                   Content-Type: text/html\r\n\r\n<p>Hello</p>";
        let parsed = parse_mail(raw.as_bytes()).unwrap();
        let body = extract_text_body(&parsed);
        assert!(body.contains("Hello"));
    }

    #[test]
    fn test_guess_mime_type() {
        let ct = guess_mime_type("report.pdf");
        assert!(format!("{ct:?}").contains("pdf"));
        let ct = guess_mime_type("image.png");
        assert!(format!("{ct:?}").contains("png"));
        let ct = guess_mime_type("unknown.xyz");
        assert!(format!("{ct:?}").contains("octet-stream"));
        let ct = guess_mime_type("noext");
        assert!(format!("{ct:?}").contains("octet-stream"));
    }

    #[test]
    fn test_trim_seen_uids() {
        let mut seen: HashSet<u32> = (0..3000).collect();
        trim_seen_uids(&mut seen);
        assert!(seen.len() <= SEEN_UIDS_MAX);
    }

    #[test]
    fn test_adapter_new_and_disconnect() {
        let config = EmailConfig::default();
        let adapter = EmailAdapter::new(config);
        assert!(!adapter.is_connected());
        adapter.disconnect();
        assert!(!adapter.is_connected());
    }
}
