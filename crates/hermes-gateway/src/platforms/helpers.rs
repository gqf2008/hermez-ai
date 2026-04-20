//! Shared helper classes for gateway platform adapters.
//!
//! Mirrors Python `gateway/platforms/helpers.py`.
//! Extracts common patterns duplicated across adapters:
//! message deduplication, text batch aggregation, markdown stripping,
//! thread participation tracking, and phone number redaction.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let mut seen = self.seen.lock().unwrap();
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
        self.seen.lock().unwrap().clear();
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
            let mut pending = self.pending.lock().unwrap();
            if let Some(existing) = pending.get_mut(&key) {
                let merged = format!("{}\n{}", existing.text(), event.text());
                existing.set_text(merged);
            } else {
                pending.insert(key.clone(), event);
            }
        }
        {
            let mut last_lens = self.last_chunk_len.lock().unwrap();
            last_lens.insert(key.clone(), chunk_len);
        }

        // Cancel prior flush timer, start a new one
        let mut tasks = self.pending_tasks.lock().unwrap();
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
                let lens = last_chunk_len.lock().unwrap();
                *lens.get(&task_key).unwrap_or(&0)
            };
            let delay = if last_len >= split_threshold {
                split_delay
            } else {
                batch_delay
            };
            tokio::time::sleep(tokio::time::Duration::from_secs_f64(delay)).await;

            let evt = {
                let mut p = pending.lock().unwrap();
                p.remove(&task_key)
            };
            if let Some(evt) = evt {
                let _ = handler(evt, task_key.clone()).await;
            }
            let mut tasks = pending_tasks.lock().unwrap();
            tasks.remove(&task_key);
            let mut lens = last_chunk_len.lock().unwrap();
            lens.remove(&task_key);
        });
        tasks.insert(key, task);
    }

    /// Cancel all pending flush tasks.
    pub fn cancel_all(&self) {
        let mut tasks = self.pending_tasks.lock().unwrap();
        for (_, task) in tasks.drain() {
            task.abort();
        }
        let mut pending = self.pending.lock().unwrap();
        pending.clear();
        let mut lens = self.last_chunk_len.lock().unwrap();
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
            let set = self.threads.lock().unwrap();
            set.iter().cloned().collect()
        };
        let to_save = if threads.len() > self.max_tracked {
            let trimmed: Vec<String> = threads.into_iter().rev().take(self.max_tracked).collect();
            {
                let mut set = self.threads.lock().unwrap();
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
        let mut set = self.threads.lock().unwrap();
        if !set.contains(thread_id) {
            set.insert(thread_id.to_string());
            drop(set);
            self._save();
        }
    }

    /// Check if *thread_id* has been participated in.
    pub fn contains(&self, thread_id: &str) -> bool {
        self.threads.lock().unwrap().contains(thread_id)
    }

    /// Clear all tracked threads.
    pub fn clear(&self) {
        self.threads.lock().unwrap().clear();
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
        cleaned = regex::Regex::new(r"\n{3,}").unwrap().replace_all(&cleaned, "\n\n").into_owned();
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
            r#"[`"']?MEDIA:\s*(?P<path>`[^`\n]+`|"[^"\n]+"|'[^'\n]+'|(?:~/|/)(?:[\w.\-]+/)*[\w.\-]+\.(?:png|jpe?g|gif|webp|mp4|mov|avi|mkv|webm|ogg|opus|mp3|wav|m4a)(?=[\s`"',;:)\]}]|$)|\S+)[`"']?"#
        ).unwrap()
    });

    let mut media = Vec::new();
    for cap in media_re.captures_iter(content) {
        if let Some(path_match) = cap.name("path") {
            let mut path = path_match.as_str().trim().to_string();
            // Strip surrounding quotes/backticks
            if path.len() >= 2 {
                let first = path.chars().next().unwrap();
                let last = path.chars().last().unwrap();
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
        cleaned = regex::Regex::new(r"\n{3,}").unwrap().replace_all(&cleaned, "\n\n").into_owned();
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
            r"(?<![/:\w.])(?:~/|/)(?:[\w.\-]+/)*[\w.\-]+\.(?:png|jpg|jpeg|gif|webp|mp4|mov|avi|mkv|webm)\b"
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
        let fence_open = regex::Regex::new(r"```(\w*)").unwrap();
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
}
