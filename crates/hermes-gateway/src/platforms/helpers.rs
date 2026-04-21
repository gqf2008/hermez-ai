//! Shared helper classes for gateway platform adapters.
//!
//! Mirrors Python `gateway/platforms/helpers.py`.
//! Extracts common patterns duplicated across adapters:
//! message deduplication, text batch aggregation, markdown stripping,
//! thread participation tracking, and phone number redaction.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Message Deduplication
// ---------------------------------------------------------------------------

/// TTL-based message deduplication cache.
///
/// Replaces identical `_seen_messages` / `_is_duplicate()` patterns
/// previously duplicated in discord, slack, dingtalk, wecom, weixin,
/// mattermost, and feishu adapters.
#[derive(Debug, Clone)]
pub struct MessageDeduplicator {
    seen: Arc<Mutex<HashMap<String, f64>>>,
    max_size: usize,
    ttl_seconds: f64,
}

impl Default for MessageDeduplicator {
    fn default() -> Self {
        Self::new(2000, 300.0)
    }
}

impl MessageDeduplicator {
    /// Create a new deduplicator with the given max size and TTL.
    pub fn new(max_size: usize, ttl_seconds: f64) -> Self {
        Self {
            seen: Arc::new(Mutex::new(HashMap::new())),
            max_size,
            ttl_seconds,
        }
    }

    /// Return `true` if *msg_id* was already seen within the TTL window.
    pub fn is_duplicate(&self, msg_id: &str) -> bool {
        if msg_id.is_empty() {
            return false;
        }
        let now = now_secs();
        let mut seen = self.seen.lock();
        if let Some(&timestamp) = seen.get(msg_id) {
            if now - timestamp < self.ttl_seconds {
                return true;
            }
            // Entry has expired — remove it and treat as new
            seen.remove(msg_id);
        }
        seen.insert(msg_id.to_string(), now);
        if seen.len() > self.max_size {
            let cutoff = now - self.ttl_seconds;
            seen.retain(|_, v| *v > cutoff);
        }
        false
    }

    /// Clear all tracked messages.
    pub fn clear(&self) {
        self.seen.lock().clear();
    }
}

// ---------------------------------------------------------------------------
// Text Batch Aggregation
// ---------------------------------------------------------------------------

/// Event that can be batched by the [`TextBatchAggregator`].
pub trait BatchableEvent: Send + 'static {
    fn text(&self) -> &str;
    fn set_text(&mut self, text: String);
    fn clone_box(&self) -> Box<dyn BatchableEvent>;
}

/// Aggregates rapid-fire text events into single messages.
///
/// Replaces the `_enqueue_text_event` / `_flush_text_batch` pattern
/// previously duplicated in telegram, discord, matrix, wecom, and feishu.
///
/// Usage:
/// ```ignore
/// let batcher = TextBatchAggregator::new(handler, 0.6, 2.0, 4000);
/// if msg_type == MessageType::Text && batcher.is_enabled() {
///     batcher.enqueue(event, session_key).await;
///     return;
/// }
/// ```
pub struct TextBatchAggregator<H>
where
    H: Fn(Box<dyn BatchableEvent>, String) -> tokio::task::JoinHandle<()> + Send + Sync + 'static,
{
    handler: Arc<H>,
    batch_delay_secs: f64,
    split_delay_secs: f64,
    split_threshold: usize,
    pending: Arc<Mutex<HashMap<String, Box<dyn BatchableEvent>>>>,
    pending_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    last_chunk_len: Arc<Mutex<HashMap<String, usize>>>,
}

impl<H> TextBatchAggregator<H>
where
    H: Fn(Box<dyn BatchableEvent>, String) -> tokio::task::JoinHandle<()> + Send + Sync + 'static,
{
    /// Create a new aggregator.
    pub fn new(
        handler: H,
        batch_delay_secs: f64,
        split_delay_secs: f64,
        split_threshold: usize,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
            batch_delay_secs,
            split_delay_secs,
            split_threshold,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_tasks: Arc::new(Mutex::new(HashMap::new())),
            last_chunk_len: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return `true` if batching is active (delay > 0).
    pub fn is_enabled(&self) -> bool {
        self.batch_delay_secs > 0.0
    }

    /// Add *event* to the pending batch for *key*.
    pub fn enqueue(&self,
        event: Box<dyn BatchableEvent>,
        key: String,
    ) {
        let chunk_len = event.text().len();
        {
            let mut pending = self.pending.lock();
            if let Some(existing) = pending.get_mut(&key) {
                let merged = format!("{}\n{}", existing.text(), event.text());
                existing.set_text(merged);
            } else {
                pending.insert(key.clone(), event);
            }
        }
        {
            let mut last_lens = self.last_chunk_len.lock();
            last_lens.insert(key.clone(), chunk_len);
        }

        // Cancel prior flush timer, start a new one
        let mut tasks = self.pending_tasks.lock();
        if let Some(prior) = tasks.remove(&key) {
            prior.abort();
        }
        let handler = self.handler.clone();
        let pending = self.pending.clone();
        let pending_tasks = self.pending_tasks.clone();
        let last_chunk_len = self.last_chunk_len.clone();
        let batch_delay = self.batch_delay_secs;
        let split_delay = self.split_delay_secs;
        let split_threshold = self.split_threshold;

        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let last_len = {
                let lens = last_chunk_len.lock();
                *lens.get(&task_key).unwrap_or(&0)
            };
            let delay = if last_len >= split_threshold {
                split_delay
            } else {
                batch_delay
            };
            tokio::time::sleep(tokio::time::Duration::from_secs_f64(delay)).await;

            let evt = {
                let mut p = pending.lock();
                p.remove(&task_key)
            };
            if let Some(evt) = evt {
                let _ = handler(evt, task_key.clone()).await;
            }
            let mut tasks = pending_tasks.lock();
            tasks.remove(&task_key);
            let mut lens = last_chunk_len.lock();
            lens.remove(&task_key);
        });
        tasks.insert(key, task);
    }

    /// Cancel all pending flush tasks.
    pub fn cancel_all(&self) {
        let mut tasks = self.pending_tasks.lock();
        for (_, task) in tasks.drain() {
            task.abort();
        }
        let mut pending = self.pending.lock();
        pending.clear();
        let mut lens = self.last_chunk_len.lock();
        lens.clear();
    }
}

// ---------------------------------------------------------------------------
// Markdown Stripping
// ---------------------------------------------------------------------------

/// Strip markdown formatting for plain-text platforms (SMS, iMessage, etc.).
///
/// Replaces identical `_strip_markdown()` functions previously
/// duplicated in sms.py, bluebubbles.py, and feishu.py.
pub fn strip_markdown(text: &str) -> String {
    fn re_bold() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"\*\*(.+?)\*\*").unwrap())
    }
    fn re_italic_star() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"\*(.+?)\*").unwrap())
    }
    fn re_bold_under() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"__(.+?)__").unwrap())
    }
    fn re_italic_under() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"_(.+?)_").unwrap())
    }
    fn re_code_block() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"```[a-zA-Z0-9_+-]*\n?").unwrap())
    }
    fn re_inline_code() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"`(.+?)`").unwrap())
    }
    fn re_heading() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"(?m)^#{1,6}\s+").unwrap())
    }
    fn re_link() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\([^\)]+\)").unwrap())
    }
    fn re_multi_newline() -> &'static regex::Regex {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap())
    }

    let mut text = re_bold().replace_all(text, "${1}").into_owned();
    text = re_italic_star().replace_all(&text, "${1}").into_owned();
    text = re_bold_under().replace_all(&text, "${1}").into_owned();
    text = re_italic_under().replace_all(&text, "${1}").into_owned();
    text = re_code_block().replace_all(&text, "").into_owned();
    text = re_inline_code().replace_all(&text, "${1}").into_owned();
    text = re_heading().replace_all(&text, "").into_owned();
    text = re_link().replace_all(&text, "${1}").into_owned();
    text = re_multi_newline().replace_all(&text, "\n\n").into_owned();
    text.trim().to_string()
}

// ---------------------------------------------------------------------------
// Thread Participation Tracking
// ---------------------------------------------------------------------------

/// Persistent tracking of threads the bot has participated in.
///
/// Replaces `_load/_save_participated_threads` + `_mark_thread_participated`
/// patterns previously duplicated in discord and matrix.
#[derive(Debug, Clone)]
pub struct ThreadParticipationTracker {
    platform: String,
    max_tracked: usize,
    threads: Arc<Mutex<HashSet<String>>>,
}

impl ThreadParticipationTracker {
    /// Create a new tracker for the given platform.
    pub fn new(platform: &str, max_tracked: usize) -> Self {
        let threads = Self::_load(platform, max_tracked);
        Self {
            platform: platform.to_string(),
            max_tracked,
            threads: Arc::new(Mutex::new(threads)),
        }
    }

    fn _state_path(platform: &str) -> PathBuf {
        hermes_core::get_hermes_home().join(format!("{platform}_threads.json"))
    }

    fn _load(platform: &str, max_tracked: usize) -> HashSet<String> {
        let path = Self::_state_path(platform);
        if !path.is_file() {
            return HashSet::new();
        }
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return HashSet::new(),
        };
        let threads: Vec<String> = match serde_json::from_str(&data) {
            Ok(t) => t,
            Err(_) => return HashSet::new(),
        };
        let set: HashSet<String> = threads.iter().cloned().collect();
        if threads.len() > max_tracked {
            let subset: HashSet<String> = threads.into_iter().rev().take(max_tracked).collect();
            return subset;
        }
        set
    }

    fn _save(&self) {
        let threads: Vec<String> = {
            let set = self.threads.lock();
            set.iter().cloned().collect()
        };
        let to_save = if threads.len() > self.max_tracked {
            let trimmed: Vec<String> = threads.into_iter().rev().take(self.max_tracked).collect();
            {
                let mut set = self.threads.lock();
                *set = trimmed.iter().cloned().collect();
            }
            trimmed
        } else {
            threads
        };
        let path = Self::_state_path(&self.platform);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("Failed to create thread state dir: {e}");
                return;
            }
        }
        match serde_json::to_vec_pretty(&to_save) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    tracing::warn!("Failed to write thread state to {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("Failed to serialize thread state: {e}"),
        }
    }

    /// Mark *thread_id* as participated and persist.
    pub fn mark(&self, thread_id: &str) {
        let mut set = self.threads.lock();
        if !set.contains(thread_id) {
            set.insert(thread_id.to_string());
            drop(set);
            self._save();
        }
    }

    /// Check if *thread_id* has been participated in.
    pub fn contains(&self, thread_id: &str) -> bool {
        self.threads.lock().contains(thread_id)
    }

    /// Clear all tracked threads.
    pub fn clear(&self) {
        self.threads.lock().clear();
        let path = Self::_state_path(&self.platform);
        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// Phone Number Redaction
// ---------------------------------------------------------------------------

/// Redact a phone number for logging, preserving the first 4 and last 4 digits.
///
/// Replaces identical `_redact_phone()` functions in signal.py, sms.py,
/// and bluebubbles.py.
pub fn redact_phone(phone: &str) -> String {
    if phone.is_empty() {
        return "<none>".into();
    }
    if phone.len() <= 8 {
        if phone.len() > 4 {
            format!("{}****{}", &phone[..2], &phone[phone.len() - 2..])
        } else {
            "****".into()
        }
    } else {
        format!("{}****{}", &phone[..4], &phone[phone.len() - 4..])
    }
}

// ---------------------------------------------------------------------------
// Media extraction (ported from Python base.py)
// ---------------------------------------------------------------------------

/// Extract image URLs from markdown `![alt](url)` and HTML `<img src="url">` tags.
///
/// Returns `(list of (url, alt_text), cleaned_content)`.
pub fn extract_images(content: &str) -> (Vec<(String, String)>, String) {
    let mut images = Vec::new();
    let mut cleaned = content.to_string();

    // Markdown images: ![alt](url)
    static MD_IMG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let md_re = MD_IMG.get_or_init(|| {
        regex::Regex::new(r"!\[([^\]]*)\]\((https?://[^\s\)]+)\)").unwrap()
    });

    for cap in md_re.captures_iter(content) {
        let alt = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let url = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        let lower = url.to_lowercase();
        if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp")
            || lower.contains("fal.media")
            || lower.contains("fal-cdn")
            || lower.contains("replicate.delivery")
        {
            images.push((url.clone(), alt));
        }
    }

    // HTML img tags
    static HTML_IMG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let html_re = HTML_IMG.get_or_init(|| {
        regex::Regex::new(r#"<img\s+src=["\']?(https?://[^\s"\'<>]+)["\']?\s*/?>\s*(?:</img>)?"#).unwrap()
    });

    for cap in html_re.captures_iter(content) {
        if let Some(url) = cap.get(1) {
            images.push((url.as_str().to_string(), String::new()));
        }
    }

    if !images.is_empty() {
        let extracted: std::collections::HashSet<String> = images.iter().map(|(u, _)| u.clone()).collect();
        cleaned = md_re.replace_all(&cleaned, |caps: &regex::Captures| {
            let url = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            if extracted.contains(url) { "".to_string() } else { caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string() }
        }).into_owned();
        cleaned = html_re.replace_all(&cleaned, |caps: &regex::Captures| {
            let url = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if extracted.contains(url) { "".to_string() } else { caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string() }
        }).into_owned();
        static MULTI_NL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = MULTI_NL.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap());
        cleaned = re.replace_all(&cleaned, "\n\n").into_owned();
        cleaned = cleaned.trim().to_string();
    }

    (images, cleaned)
}

/// Extract `MEDIA:<path>` tags and `[[audio_as_voice]]` directives from text.
///
/// Returns `(list of (path, is_voice), cleaned_content)`.
pub fn extract_media(content: &str) -> (Vec<(String, bool)>, String) {
    let has_voice_tag = content.contains("[[audio_as_voice]]");
    let mut cleaned = content.replace("[[audio_as_voice]]", "");

    static MEDIA_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let media_re = MEDIA_RE.get_or_init(|| {
        regex::Regex::new(
            r#"[`"']?MEDIA:\s*(?P<path>`[^`\n]+`|"[^"\n]+"|'[^'\n]+'|(?:~/|/)(?:[\w.\-]+/)*[\w.\-]+\.(?:png|jpe?g|gif|webp|mp4|mov|avi|mkv|webm|ogg|opus|mp3|wav|m4a)|\S+)[`"']?"#
        ).unwrap()
    });

    let mut media = Vec::new();
    for cap in media_re.captures_iter(content) {
        if let Some(path_match) = cap.name("path") {
            let mut path = path_match.as_str().trim().to_string();
            // Strip surrounding quotes/backticks
            if path.len() >= 2 {
                let mut chars = path.chars();
                let first = chars.next().unwrap_or(' ');
                let last = chars.next_back().unwrap_or(' ');
                if first == last && (first == '`' || first == '"' || first == '\'') {
                    path = path[1..path.len()-1].trim().to_string();
                }
            }
            path = path.trim_start_matches("`\"'").trim_end_matches("`\"',.;:)}]").to_string();
            if !path.is_empty() {
                media.push((path, has_voice_tag));
            }
        }
    }

    if !media.is_empty() {
        cleaned = media_re.replace_all(&cleaned, "").into_owned();
        static MULTI_NL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = MULTI_NL.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap());
        cleaned = re.replace_all(&cleaned, "\n\n").into_owned();
        cleaned = cleaned.trim().to_string();
    }

    (media, cleaned)
}

/// Detect bare local file paths in response text for native media delivery.
///
/// Matches absolute paths (`/...`) and tilde paths (`~/...`) ending in
/// common image or video extensions. Ignores paths inside code blocks.
///
/// Returns `(list of expanded file paths, cleaned_text)`.
pub fn extract_local_files(content: &str) -> (Vec<String>, String) {
    static PATH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let path_re = PATH_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:~/|/)(?:[\w.\-]+/)*[\w.\-]+\.(?:png|jpg|jpeg|gif|webp|mp4|mov|avi|mkv|webm)\b"
        ).unwrap()
    });

    // Collect code block spans
    let mut code_spans: Vec<(usize, usize)> = Vec::new();
    static FENCE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let fence_re = FENCE_RE.get_or_init(|| regex::Regex::new(r"```[^\n]*\n.*?```").unwrap());
    for m in fence_re.find_iter(content) {
        code_spans.push((m.start(), m.end()));
    }
    static INLINE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let inline_re = INLINE_RE.get_or_init(|| regex::Regex::new(r"`[^`\n]+`").unwrap());
    for m in inline_re.find_iter(content) {
        code_spans.push((m.start(), m.end()));
    }

    fn in_code(pos: usize, spans: &[(usize, usize)]) -> bool {
        spans.iter().any(|(s, e)| *s <= pos && pos < *e)
    }

    let mut found: Vec<(String, String)> = Vec::new(); // (raw, expanded)
    for m in path_re.find_iter(content) {
        if in_code(m.start(), &code_spans) {
            continue;
        }
        let raw = m.as_str().to_string();
        // Skip URL paths (e.g. https://example.com/img.png)
        if raw.starts_with("//") {
            continue;
        }
        let expanded = if raw.starts_with("~/") {
            std::env::var("HOME")
                .map(|h| std::path::Path::new(&h).join(&raw[2..]).to_string_lossy().into_owned())
                .unwrap_or_else(|_| raw.clone())
        } else {
            raw.clone()
        };
        found.push((raw, expanded));
    }

    let mut cleaned = content.to_string();
    for (raw, _) in &found {
        cleaned = cleaned.replace(raw, "");
    }
    cleaned = regex::Regex::new(r"\n{3,}").unwrap().replace_all(&cleaned, "\n\n").into_owned();
    cleaned = cleaned.trim().to_string();

    let paths: Vec<String> = found.into_iter().map(|(_, p)| p).collect();
    (paths, cleaned)
}

/// Truncate a long message into chunks, preserving code block boundaries.
///
/// When a split falls inside a triple-backtick code block, the fence is
/// closed at the end of the current chunk and reopened at the start of the next.
pub fn truncate_message(content: &str, max_length: usize) -> Vec<String> {
    if content.len() <= max_length {
        return vec![content.to_string()];
    }

    const INDICATOR_RESERVE: usize = 10; // room for " (XX/XX)"
    const FENCE_CLOSE: &str = "\n```";

    let mut chunks = Vec::new();
    let mut remaining = content;
    let mut carry_lang: Option<String> = None;
    static FENCE_OPEN_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let fence_open = FENCE_OPEN_RE.get_or_init(|| regex::Regex::new(r"```(\w*)").unwrap());

    while !remaining.is_empty() {
        let prefix = match &carry_lang {
            Some(lang) => format!("```{lang}\n"),
            None => String::new(),
        };

        let headroom = max_length.saturating_sub(INDICATOR_RESERVE + prefix.len() + FENCE_CLOSE.len());
        let headroom = headroom.max(max_length / 2);

        if prefix.len() + remaining.len() <= max_length - INDICATOR_RESERVE {
            chunks.push(prefix + remaining);
            break;
        }

        let region = &remaining[..headroom.min(remaining.len())];
        let mut split_at = region.rfind('\n').unwrap_or(0);
        if split_at < headroom / 2 {
            split_at = region.rfind(' ').unwrap_or(0);
        }
        if split_at < 1 {
            split_at = headroom;
        }

        let mut chunk = prefix + &remaining[..split_at];
        remaining = &remaining[split_at..];

        // Detect if we're inside a code block
        let opens: Vec<_> = fence_open.find_iter(&chunk).collect();
        let _closes = chunk.matches("```").count();
        let open_count = opens.len();
        let in_block = open_count % 2 == 1;

        if in_block {
            let lang = opens.last().and_then(|m| {
                fence_open.captures(m.as_str()).and_then(|c| c.get(1).map(|g| g.as_str().to_string()))
            });
            chunk.push_str(FENCE_CLOSE);
            carry_lang = lang;
        } else {
            carry_lang = None;
        }

        chunks.push(chunk);
    }

    // Add chunk indicators
    let total = chunks.len();
    if total > 1 {
        for (i, chunk) in chunks.iter_mut().enumerate() {
            *chunk = format!("{} ({}/{total})", chunk.trim_end(), i + 1);
        }
    }

    chunks
}

// ---------------------------------------------------------------------------
// Internal utilities
// ---------------------------------------------------------------------------

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ---------------------------------------------------------------------------
// URL / Network helpers (mirrors Python base.py)
// ---------------------------------------------------------------------------

/// Return a URL string safe for logs (no query/fragment/userinfo).
pub fn safe_url_for_log(url: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    if url.is_empty() {
        return String::new();
    }

    let safe = match url.parse::<reqwest::Url>() {
        Ok(parsed) if parsed.host().is_some() => {
            let netloc = parsed.host_str().unwrap_or("");
            let base = format!("{}://{}", parsed.scheme(), netloc);
            let path = parsed.path();
            if !path.is_empty() && path != "/" {
                if let Some(basename) = path.rsplit('/').next() {
                    if !basename.is_empty() {
                        format!("{}/.../{}", base, basename)
                    } else {
                        format!("{}/...", base)
                    }
                } else {
                    format!("{}/...", base)
                }
            } else {
                base
            }
        }
        _ => url.to_string(),
    };

    if safe.len() <= max_len {
        safe
    } else if max_len <= 3 {
        "...".to_string()
    } else {
        format!("{}...", &safe[..max_len - 3])
    }
}

/// Return `true` if *host* would expose the server beyond loopback.
///
/// Loopback addresses are local-only. Unspecified addresses bind all
/// interfaces. Hostnames are resolved; DNS failure fails closed (returns
/// `true` to be permissive).
pub fn is_network_accessible(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(addr) => {
            if addr.is_loopback() {
                return false;
            }
            // IPv4-mapped loopback (::ffff:127.0.0.1)
            if let IpAddr::V6(v6) = addr {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    if v4.is_loopback() {
                        return false;
                    }
                }
            }
            true
        }
        Err(_) => {
            // Hostname — try to resolve
            match (host, 0).to_socket_addrs() {
                Ok(mut addrs) => addrs.any(|a| !a.ip().is_loopback()),
                Err(_) => true, // DNS failure — fail permissive
            }
        }
    }
}

/// Resolve proxy URL from environment variables or macOS system proxy.
///
/// Check order:
/// 1. *platform_env_var* (e.g. `TELEGRAM_PROXY`) — highest priority
/// 2. `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (and lowercase variants)
/// 3. macOS system proxy via `scutil --proxy` (auto-detect)
pub fn resolve_proxy_url(platform_env_var: Option<&str>) -> Option<String> {
    if let Some(var) = platform_env_var {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }

    for key in [
        "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY",
        "https_proxy", "http_proxy", "all_proxy",
    ] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(proxy) = detect_macos_system_proxy() {
            return Some(proxy);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn detect_macos_system_proxy() -> Option<String> {
    let output = std::process::Command::new("scutil")
        .args(["--proxy"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut props = HashMap::new();
    for line in text.lines() {
        if let Some((key, val)) = line.split_once(" : ") {
            props.insert(key.trim(), val.trim());
        }
    }
    for (enable_key, host_key, port_key) in [
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort"),
        ("HTTPEnable", "HTTPProxy", "HTTPPort"),
    ] {
        if props.get(enable_key) == Some(&"1") {
            let host = props.get(host_key).copied()?;
            let port = props.get(port_key).copied()?;
            return Some(format!("http://{}:{}", host, port));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// UTF-16 length helpers
// ---------------------------------------------------------------------------

/// Return the UTF-16 code-unit length of a string.
///
/// Some platforms (e.g. Telegram) count message length in UTF-16 code units
/// rather than Unicode scalar values or bytes.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Return the longest prefix of *s* whose UTF-16 length is <= *limit*.
pub fn prefix_within_utf16_limit(s: &str, limit: usize) -> &str {
    if utf16_len(s) <= limit {
        return s;
    }
    let mut byte_idx = 0;
    let mut units = 0;
    for ch in s.chars() {
        let ch_units = ch.encode_utf16(&mut [0; 2]).len();
        if units + ch_units > limit {
            break;
        }
        units += ch_units;
        byte_idx += ch.len_utf8();
    }
    &s[..byte_idx]
}

/// Return the largest codepoint offset *n* such that `len_fn(&s[..n]) <= budget`.
///
/// Used for truncation when *len_fn* measures length in units different
/// from Rust char boundaries (e.g. UTF-16 code units). Falls back to
/// binary search which is O(log n) calls to *len_fn*.
pub fn custom_unit_truncate(s: &str, budget: usize, len_fn: impl Fn(&str) -> usize) -> usize {
    let char_count = s.chars().count();
    if len_fn(s) <= budget {
        return char_count;
    }
    let mut lo = 0;
    let mut hi = char_count;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let byte_offset = s.char_indices().nth(mid).map(|(i, _)| i).unwrap_or(s.len());
        if len_fn(&s[..byte_offset]) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ---------------------------------------------------------------------------
// Pending message merge (mirrors Python merge_pending_message_event)
// ---------------------------------------------------------------------------

/// Types of incoming messages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MessageType {
    #[default]
    Text,
    Location,
    Photo,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    Command,
}

/// A pending message event that can be merged with subsequent events.
///
/// Photo bursts / albums often arrive as multiple near-simultaneous PHOTO
/// events. Merge those into the existing queued event so the next turn sees
/// the whole burst.
#[derive(Debug, Clone, Default)]
pub struct PendingMessageEvent {
    pub text: String,
    pub message_type: MessageType,
    pub media_urls: Vec<String>,
    pub media_types: Vec<String>,
}

/// Merge an incoming event into the pending map for a session.
///
/// When *merge_text* is enabled, rapid follow-up TEXT events are appended
/// instead of replacing the pending turn.
pub fn merge_pending_message(
    pending: &mut HashMap<String, PendingMessageEvent>,
    session_key: &str,
    event: PendingMessageEvent,
    merge_text: bool,
) {
    if let Some(existing) = pending.get_mut(session_key) {
        let existing_is_photo = existing.message_type == MessageType::Photo;
        let incoming_is_photo = event.message_type == MessageType::Photo;
        let existing_has_media = !existing.media_urls.is_empty();
        let incoming_has_media = !event.media_urls.is_empty();

        if existing_is_photo && incoming_is_photo {
            existing.media_urls.extend(event.media_urls);
            existing.media_types.extend(event.media_types);
            if !event.text.is_empty() {
                existing.text = merge_caption(Some(&existing.text), &event.text);
            }
            return;
        }

        if existing_has_media || incoming_has_media {
            if incoming_has_media {
                existing.media_urls.extend(event.media_urls);
                existing.media_types.extend(event.media_types);
            }
            if !event.text.is_empty() {
                existing.text = merge_caption(Some(&existing.text), &event.text);
            }
            if existing_is_photo || incoming_is_photo {
                existing.message_type = MessageType::Photo;
            }
            return;
        }

        if merge_text
            && existing.message_type == MessageType::Text
            && event.message_type == MessageType::Text
        {
            if !event.text.is_empty() {
                existing.text = if existing.text.is_empty() {
                    event.text.clone()
                } else {
                    format!("{}\n{}", existing.text, event.text)
                };
            }
            return;
        }
    }

    pending.insert(session_key.to_string(), event);
}

/// Merge two caption strings, avoiding double spaces.
pub fn merge_caption(existing: Option<&str>, new_text: &str) -> String {
    match existing {
        None => new_text.to_string(),
        Some(old) => {
            let old = old.trim_end();
            let new = new_text.trim_start();
            if old.is_empty() {
                new.to_string()
            } else if new.is_empty() {
                old.to_string()
            } else {
                format!("{} {}", old, new)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel prompt resolution
// ---------------------------------------------------------------------------

/// Resolve a per-channel ephemeral prompt from platform config.
///
/// Looks up `channel_prompts` in the adapter's config extra map.
/// Prefers an exact match on *channel_id*; falls back to *parent_id*
/// (useful for forum threads / child channels inheriting a parent prompt).
///
/// Returns the prompt string, or None if no match is found. Blank/whitespace-
/// only prompts are treated as absent.
pub fn resolve_channel_prompt(
    config_extra: &serde_json::Map<String, serde_json::Value>,
    channel_id: &str,
    parent_id: Option<&str>,
) -> Option<String> {
    let prompts = config_extra
        .get("channel_prompts")
        .and_then(|v| v.as_object())?;

    for key in [Some(channel_id), parent_id] {
        let key = key?;
        if let Some(prompt) = prompts.get(key) {
            let prompt = prompt.as_str()?.trim();
            if !prompt.is_empty() {
                return Some(prompt.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SSRF URL safety checks (mirrors Python tools/url_safety.py)
// ---------------------------------------------------------------------------

/// Return `true` if *url* is safe to fetch (not a private/internal address).
///
/// Blocks requests to loopback, link-local, private, reserved, multicast,
/// unspecified, and CGNAT (100.64.0.0/10) addresses. Also blocks known
/// internal hostnames like `metadata.google.internal`.
///
/// DNS resolution failures fail closed (return `false`).
pub fn is_safe_url(url: &str) -> bool {
    let parsed = match url.parse::<reqwest::Url>() {
        Ok(u) => u,
        Err(_) => return false,
    };

    let scheme = parsed.scheme().to_lowercase();

    let hostname = match parsed.host() {
        Some(url::Host::Domain(d)) => d.to_lowercase(),
        Some(url::Host::Ipv4(ip)) => {
            return !is_blocked_ip(std::net::IpAddr::V4(ip));
        }
        Some(url::Host::Ipv6(ip)) => {
            return !is_blocked_ip(std::net::IpAddr::V6(ip));
        }
        None => return false,
    };

    // Block known internal hostnames
    const BLOCKED_HOSTNAMES: &[&str] = &["metadata.google.internal", "metadata.goog"];
    if BLOCKED_HOSTNAMES.contains(&hostname.as_str()) {
        return false;
    }

    // Trusted HTTPS hostnames that may resolve to private IPs
    const TRUSTED_PRIVATE_IP_HOSTS: &[&str] = &["multimedia.nt.qq.com.cn"];
    let allow_private_ip = scheme == "https" && TRUSTED_PRIVATE_IP_HOSTS.contains(&hostname.as_str());

    // Try to resolve and check IP
    let addrs = match (hostname.as_str(), 0).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(_) => return false, // DNS failure — fail closed
    };

    for addr in addrs {
        let ip = addr.ip();
        if !allow_private_ip && is_blocked_ip(ip) {
            return false;
        }
    }

    true
}

fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || is_cgnat_v4(v4)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

fn is_cgnat_v4(ip: std::net::Ipv4Addr) -> bool {
    // 100.64.0.0/10 — RFC 6598 CGNAT / Shared Address Space
    let octets = ip.octets();
    octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127
}

// ---------------------------------------------------------------------------
// Media type detection
// ---------------------------------------------------------------------------

/// Return `true` if *data* starts with a known image magic-byte sequence.
pub fn looks_like_image(data: &[u8]) -> bool {
    if data.len() < 2 {
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
    if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Media cache utilities
// ---------------------------------------------------------------------------

fn media_cache_dir(subdir: &str) -> PathBuf {
    let dir = hermes_core::get_hermes_home().join("cache").join(subdir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Save raw image bytes to the cache and return the absolute file path.
///
/// Returns an error if *data* does not look like a valid image.
pub fn cache_image_from_bytes(data: &[u8], ext: &str) -> Result<PathBuf, String> {
    if !looks_like_image(data) {
        let snippet = String::from_utf8_lossy(&data[..data.len().min(80)]);
        return Err(format!(
            "Refusing to cache non-image data as {ext} (starts with: {snippet:?})"
        ));
    }
    let cache_dir = media_cache_dir("images");
    let filename = format!("img_{}{ext}", uuid::Uuid::new_v4().simple());
    let filepath = cache_dir.join(&filename);
    std::fs::write(&filepath, data).map_err(|e| format!("Failed to write image cache: {e}"))?;
    Ok(filepath)
}

/// Save raw audio bytes to the cache and return the absolute file path.
pub fn cache_audio_from_bytes(data: &[u8], ext: &str) -> Result<PathBuf, String> {
    let cache_dir = media_cache_dir("audio");
    let filename = format!("audio_{}{ext}", uuid::Uuid::new_v4().simple());
    let filepath = cache_dir.join(&filename);
    std::fs::write(&filepath, data).map_err(|e| format!("Failed to write audio cache: {e}"))?;
    Ok(filepath)
}

/// Save raw document bytes to the cache and return the absolute file path.
///
/// The cached filename preserves the original human-readable name with a
/// unique prefix: `doc_{uuid12}_{original_filename}`.
pub fn cache_document_from_bytes(data: &[u8], filename: &str) -> Result<PathBuf, String> {
    let cache_dir = media_cache_dir("documents");
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document")
        .replace('\x00', "")
        .trim()
        .to_string();
    let safe_name = if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
        "document".to_string()
    } else {
        safe_name
    };
    let cached_name = format!("doc_{}_{safe_name}", uuid::Uuid::new_v4().simple());
    let filepath = cache_dir.join(&cached_name);
    // Safety check: ensure path stays inside cache dir
    if !filepath
        .canonicalize()
        .unwrap_or_else(|_| filepath.clone())
        .starts_with(cache_dir.canonicalize().unwrap_or_else(|_| cache_dir.clone()))
    {
        return Err(format!("Path traversal rejected: {filename:?}"));
    }
    std::fs::write(&filepath, data).map_err(|e| format!("Failed to write document cache: {e}"))?;
    Ok(filepath)
}

/// Delete cached files older than *max_age_hours* from a cache directory.
///
/// Returns the number of files removed.
pub fn cleanup_media_cache(subdir: &str, max_age_hours: u64) -> usize {
    let cache_dir = hermes_core::get_hermes_home().join("cache").join(subdir);
    let cutoff = now_secs() - (max_age_hours as f64 * 3600.0);
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    if let Ok(mtime) = meta.modified() {
                        let mtime_secs = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64();
                        if mtime_secs < cutoff {
                            let _ = std::fs::remove_file(entry.path());
                            removed += 1;
                        }
                    }
                }
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deduplicator_basic() {
        let dedup = MessageDeduplicator::new(10, 60.0);
        assert!(!dedup.is_duplicate("msg1"));
        assert!(dedup.is_duplicate("msg1"));
        assert!(!dedup.is_duplicate("msg2"));
    }

    #[test]
    fn test_deduplicator_empty_id() {
        let dedup = MessageDeduplicator::default();
        assert!(!dedup.is_duplicate(""));
        assert!(!dedup.is_duplicate(""));
    }

    #[test]
    fn test_deduplicator_clear() {
        let dedup = MessageDeduplicator::default();
        assert!(!dedup.is_duplicate("a"));
        assert!(dedup.is_duplicate("a"));
        dedup.clear();
        assert!(!dedup.is_duplicate("a"));
    }

    #[test]
    fn test_strip_markdown() {
        let text = "**bold** *italic* `code` [link](http://x.com)\n# heading\n\n\nmore";
        let result = strip_markdown(text);
        assert!(result.contains("bold"));
        assert!(result.contains("italic"));
        assert!(result.contains("code"));
        assert!(result.contains("link"));
        assert!(!result.contains("**"));
        assert!(!result.contains("`"));
        assert!(!result.contains("# heading"));
        assert!(!result.contains("\n\n\n"));
    }

    #[test]
    fn test_strip_markdown_code_block() {
        let text = "```rust\nfn main() {}\n```";
        let result = strip_markdown(text);
        assert!(!result.contains("```rust"));
        assert!(result.contains("fn main()"));
    }

    #[test]
    fn test_redact_phone() {
        assert_eq!(redact_phone("+8613800138000"), "+861****8000");
        assert_eq!(redact_phone("12345678"), "12****78");
        assert_eq!(redact_phone("1234"), "****");
        assert_eq!(redact_phone(""), "<none>");
    }

    #[test]
    fn test_thread_tracker_contains() {
        // Clean up any stale state from previous runs
        let _ = std::fs::remove_file(ThreadParticipationTracker::_state_path("test_platform"));
        let tracker = ThreadParticipationTracker::new("test_platform", 10);
        assert!(!tracker.contains("thread_1"));
        tracker.mark("thread_1");
        assert!(tracker.contains("thread_1"));
        // Cleanup
        let _ = std::fs::remove_file(ThreadParticipationTracker::_state_path("test_platform"));
    }

    #[test]
    fn test_thread_tracker_clear() {
        let _ = std::fs::remove_file(ThreadParticipationTracker::_state_path("test_platform2"));
        let tracker = ThreadParticipationTracker::new("test_platform2", 10);
        tracker.mark("t1");
        assert!(tracker.contains("t1"));
        tracker.clear();
        assert!(!tracker.contains("t1"));
    }

    #[test]
    fn test_text_batch_aggregator_disabled() {
        let agg = TextBatchAggregator::new(
            |_evt, _key| tokio::spawn(async {}),
            0.0,
            2.0,
            4000,
        );
        assert!(!agg.is_enabled());
    }

    #[test]
    fn test_text_batch_aggregator_enabled() {
        let agg = TextBatchAggregator::new(
            |_evt, _key| tokio::spawn(async {}),
            0.6,
            2.0,
            4000,
        );
        assert!(agg.is_enabled());
    }

    #[test]
    fn test_safe_url_for_log() {
        assert_eq!(
            safe_url_for_log("https://user:pass@example.com/path/to/file.jpg?token=secret", 80),
            "https://example.com/.../file.jpg"
        );
        assert_eq!(
            safe_url_for_log("https://example.com/", 30),
            "https://example.com"
        );
        assert_eq!(
            safe_url_for_log("not-a-url", 10),
            "not-a-url"
        );
        assert_eq!(safe_url_for_log("", 10), "");
        assert_eq!(safe_url_for_log("https://example.com/very/long/path", 20), "https://example.c...");
    }

    #[test]
    fn test_is_network_accessible() {
        assert!(!is_network_accessible("127.0.0.1"));
        assert!(!is_network_accessible("::1"));
        assert!(is_network_accessible("8.8.8.8"));
        assert!(is_network_accessible("0.0.0.0"));
    }

    #[test]
    fn test_looks_like_image() {
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n"));
        assert!(looks_like_image(b"\xff\xd8\xff"));
        assert!(looks_like_image(b"GIF87a"));
        assert!(looks_like_image(b"GIF89a"));
        assert!(looks_like_image(b"BM"));
        assert!(looks_like_image(b"RIFFxxxxWEBP"));
        assert!(!looks_like_image(b"<html>"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn test_cache_image_from_bytes() {
        let png = b"\x89PNG\r\n\x1a\nfake_png_data";
        let path = cache_image_from_bytes(png, ".png").unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".png"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_cache_image_rejects_non_image() {
        let result = cache_image_from_bytes(b"<html>not an image</html>", ".jpg");
        assert!(result.is_err());
    }

    #[test]
    fn test_cache_document_from_bytes() {
        let data = b"hello world";
        let path = cache_document_from_bytes(data, "report.pdf").unwrap();
        assert!(path.exists());
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("doc_"));
        assert!(name.ends_with("_report.pdf"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_cleanup_media_cache_no_old_files() {
        // With no old files, cleanup should remove nothing
        let removed = cleanup_media_cache("images_test_empty", 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_utf16_len() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len("你好"), 2); // CJK = 1 UTF-16 unit each
        assert_eq!(utf16_len("🎉"), 2); // emoji = surrogate pair = 2 UTF-16 units
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn test_prefix_within_utf16_limit() {
        assert_eq!(prefix_within_utf16_limit("hello world", 100), "hello world");
        assert_eq!(prefix_within_utf16_limit("hello world", 5), "hello");
        // CJK chars
        assert_eq!(prefix_within_utf16_limit("你好世界", 2), "你好");
        // Emoji (2 UTF-16 units) — should be excluded if limit is 1
        assert_eq!(prefix_within_utf16_limit("a🎉b", 1), "a");
        // Emoji fits at limit 2
        assert_eq!(prefix_within_utf16_limit("a🎉b", 3), "a🎉");
    }

    #[test]
    fn test_custom_unit_truncate() {
        let len_fn = |s: &str| utf16_len(s);
        assert_eq!(custom_unit_truncate("hello world", 100, len_fn), 11);
        assert_eq!(custom_unit_truncate("hello world", 5, len_fn), 5);
        // CJK chars — each is 1 UTF-16 unit
        assert_eq!(custom_unit_truncate("你好世界", 2, len_fn), 2);
        // Emoji — 2 UTF-16 units, so limit 1 should truncate before emoji
        assert_eq!(custom_unit_truncate("a🎉b", 1, len_fn), 1);
        // limit 3 allows "a🎉"
        assert_eq!(custom_unit_truncate("a🎉b", 3, len_fn), 2); // "a🎉" = 3 units, "a🎉b" = 4 units
    }

    #[test]
    fn test_merge_caption() {
        assert_eq!(merge_caption(None, "new"), "new");
        assert_eq!(merge_caption(Some("old"), "new"), "old new");
        assert_eq!(merge_caption(Some("old "), " new"), "old new");
    }

    #[test]
    fn test_resolve_channel_prompt() {
        let mut extra = serde_json::json!({
            "channel_prompts": {
                "ch-1": "prompt-one",
                "ch-2": "  prompt-two  ",
            }
        });
        let map = extra.as_object_mut().unwrap();
        assert_eq!(resolve_channel_prompt(map, "ch-1", None), Some("prompt-one".to_string()));
        assert_eq!(resolve_channel_prompt(map, "ch-2", None), Some("prompt-two".to_string()));
        assert_eq!(resolve_channel_prompt(map, "ch-3", Some("ch-1")), Some("prompt-one".to_string()));
        assert_eq!(resolve_channel_prompt(map, "ch-3", None), None);
    }

    #[test]
    fn test_is_safe_url_blocks_private() {
        assert!(!is_safe_url("http://localhost:8080"));
        assert!(!is_safe_url("http://127.0.0.1"));
        assert!(!is_safe_url("http://192.168.1.1"));
        assert!(!is_safe_url("http://10.0.0.1"));
        assert!(!is_safe_url("http://172.16.0.1"));
        assert!(!is_safe_url("http://169.254.169.254"));
        assert!(!is_safe_url("http://[::1]"));
        assert!(!is_safe_url("http://metadata.google.internal"));
    }

    #[test]
    fn test_is_safe_url_allows_public() {
        // Public IP addresses (no DNS lookup required)
        assert!(is_safe_url("https://8.8.8.8"));
        assert!(is_safe_url("http://1.1.1.1"));
        // Trusted private-IP host allowed over HTTPS
        assert!(is_safe_url("https://multimedia.nt.qq.com.cn/file"));
    }
}
