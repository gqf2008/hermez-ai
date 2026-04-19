//! Gateway streaming consumer — bridges sync agent callbacks to async platform delivery.
//!
//! The agent fires `stream_delta_callback(text)` synchronously.
//! `GatewayStreamConsumer`:
//!   1. Receives deltas via `on_delta()` (thread-safe, sync)
//!   2. Queues them to an asyncio task via channel
//!   3. The async `run()` task buffers, rate-limits, and progressively edits
//!      a single message on the target platform

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::warn;

/// Sentinel to signal the stream is complete.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum StreamItem {
    Done,
    SegmentBreak,
    Commentary(String),
    Delta(String),
}

/// Runtime config for a single stream consumer instance.
#[derive(Debug, Clone)]
pub struct StreamConsumerConfig {
    /// Minimum interval between progressive edits.
    pub edit_interval: Duration,
    /// Buffer size threshold before forcing an edit.
    pub buffer_threshold: usize,
    /// Cursor appended during streaming to show activity.
    pub cursor: String,
}

impl Default for StreamConsumerConfig {
    fn default() -> Self {
        Self {
            edit_interval: Duration::from_secs(1),
            buffer_threshold: 40,
            cursor: " ▉".to_string(),
        }
    }
}

/// Async trait that platform adapters must implement for streaming delivery.
#[async_trait]
pub trait StreamEditTransport: Send + Sync {
    /// Send a new message. Returns the message ID.
    async fn stream_send(&self, chat_id: &str, content: &str) -> Result<String, String>;

    /// Edit an existing message by ID.
    async fn stream_edit(&self, chat_id: &str, message_id: &str, content: &str) -> Result<bool, String>;

    /// Maximum message length for this platform.
    fn max_message_length(&self) -> usize {
        4096
    }
}

/// Async consumer that progressively edits a platform message with streamed tokens.
///
/// Usage:
/// ```ignore
/// let consumer = GatewayStreamConsumer::new(adapter, chat_id, config);
/// // Pass consumer.on_delta as stream_delta_callback to AIAgent
/// // Start the consumer as a tokio task
/// tokio::spawn(consumer.run(rx));
/// // ... run agent in thread pool ...
/// consumer.finish().await; // signal completion
/// ```
#[allow(dead_code)]
pub struct GatewayStreamConsumer {
    adapter: Arc<dyn StreamEditTransport>,
    chat_id: String,
    config: StreamConsumerConfig,
    /// Sender for the delta channel (sync-safe via parking_lot or std mutex).
    sender: std::sync::Mutex<Option<mpsc::Sender<StreamItem>>>,
    /// Whether at least one message was sent.
    already_sent: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the final response was sent.
    final_response_sent: Arc<std::sync::atomic::AtomicBool>,
}

impl GatewayStreamConsumer {
    pub fn new(
        adapter: Arc<dyn StreamEditTransport>,
        chat_id: &str,
        config: StreamConsumerConfig,
    ) -> Self {
        Self {
            adapter,
            chat_id: chat_id.to_string(),
            config,
            sender: std::sync::Mutex::new(None),
            already_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            final_response_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Thread-safe callback — called from the agent's worker thread.
    pub fn on_delta(&self, text: &str) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.try_send(StreamItem::Delta(text.to_string()));
        }
    }

    /// Signal a tool boundary — finalize current message and start a new one.
    pub fn on_segment_break(&self) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.try_send(StreamItem::SegmentBreak);
        }
    }

    /// Queue a completed interim assistant commentary message.
    pub fn on_commentary(&self, text: &str) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.try_send(StreamItem::Commentary(text.to_string()));
        }
    }

    /// Signal that the stream is complete.
    pub async fn finish(&self) {
        let sender = {
            let mut guard = self.sender.lock().unwrap();
            guard.take()
        };
        if let Some(sender) = sender {
            let _ = sender.send(StreamItem::Done).await;
        }
    }

    /// Take the already_sent flag for inspection.
    pub fn already_sent(&self) -> bool {
        self.already_sent.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Take the final_response_sent flag for inspection.
    pub fn final_response_sent(&self) -> bool {
        self.final_response_sent.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Create the channel and return the receiver for `run()`.
    #[allow(dead_code)]
    fn create_channel(&self) -> mpsc::Receiver<StreamItem> {
        let (tx, rx) = mpsc::channel(256);
        *self.sender.lock().unwrap() = Some(tx);
        rx
    }

    /// Strip MEDIA: directives and internal markers from text before display.
    #[allow(dead_code)]
    fn clean_for_display(text: &str) -> String {
        if !text.contains("MEDIA:") && !text.contains("[[audio_as_voice]]") {
            return text.to_string();
        }
        let mut cleaned = text.replace("[[audio_as_voice]]", "");
        // Simple MEDIA: stripping
        let media_re = regex::Regex::new(r#"[`"']?MEDIA:\s*\S+[`"']?"#).unwrap();
        cleaned = media_re.replace_all(&cleaned, "").to_string();
        // Collapse excessive blank lines
        let multi_nl = regex::Regex::new(r"\n{3,}").unwrap();
        cleaned = multi_nl.replace_all(&cleaned, "\n\n").to_string();
        cleaned.trim_end().to_string()
    }

    /// Split text into reasonably sized chunks for fallback sends.
    #[allow(dead_code)]
    fn split_text_chunks(text: &str, limit: usize) -> Vec<String> {
        if text.len() <= limit {
            return vec![text.to_string()];
        }
        let mut chunks = Vec::new();
        let mut remaining = text;
        while remaining.len() > limit {
            let split_at = remaining[..limit.min(remaining.len())]
                .rfind('\n')
                .unwrap_or_else(|| limit.min(remaining.len()));
            let split_at = if split_at < limit / 2 {
                limit.min(remaining.len())
            } else {
                split_at
            };
            chunks.push(remaining[..split_at].to_string());
            remaining = remaining[split_at..].trim_start_matches('\n');
        }
        if !remaining.is_empty() {
            chunks.push(remaining.to_string());
        }
        chunks
    }

    /// Async task that drains the channel and edits the platform message.
    #[allow(dead_code)]
    pub(crate) async fn run(self, mut rx: mpsc::Receiver<StreamItem>) {
        let safe_limit = self.adapter.max_message_length().saturating_sub(
            self.config.cursor.len() + 100
        ).max(500);

        let mut accumulated = String::new();
        let mut message_id: Option<String> = None;
        let mut last_edit_time = Instant::now();
        let mut last_sent_text = String::new();
        let mut edit_supported = true;
        let mut flood_strikes = 0u32;
        let max_flood_strikes = 3u32;

        loop {
            // Drain all available items from the channel
            let mut got_done = false;
            let mut got_segment_break = false;
            let mut commentary_text: Option<String> = None;

            while let Ok(item) = rx.try_recv() {
                match item {
                    StreamItem::Done => {
                        got_done = true;
                        break;
                    }
                    StreamItem::SegmentBreak => {
                        got_segment_break = true;
                        break;
                    }
                    StreamItem::Commentary(text) => {
                        commentary_text = Some(text);
                        break;
                    }
                    StreamItem::Delta(text) => {
                        accumulated.push_str(&text);
                    }
                }
            }

            // Decide whether to flush an edit
            let now = Instant::now();
            let elapsed = now.duration_since(last_edit_time);
            let should_edit = got_done
                || got_segment_break
                || commentary_text.is_some()
                || (elapsed >= self.config.edit_interval && !accumulated.is_empty())
                || accumulated.len() >= self.config.buffer_threshold;

            let mut current_update_visible = false;
            if should_edit && !accumulated.is_empty() {
                // Handle overflow: split into properly sized chunks
                if accumulated.len() > safe_limit && message_id.is_none() {
                    let chunks = Self::split_text_chunks(&accumulated, safe_limit);
                    for chunk in &chunks {
                        let display = Self::clean_for_display(chunk);
                        if display.trim().is_empty() {
                            continue;
                        }
                        match self.adapter.stream_send(&self.chat_id, &display).await {
                            Ok(msg_id) => {
                                message_id = Some(msg_id);
                                self.already_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                                last_sent_text = display;
                            }
                            Err(_) => {
                                edit_supported = false;
                            }
                        }
                    }
                    accumulated.clear();
                    last_sent_text.clear();
                    last_edit_time = Instant::now();
                    if got_done {
                        self.final_response_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    if got_segment_break {
                        message_id = None;
                        continue;
                    }
                    continue;
                }

                // Existing message: edit it with accumulated text
                let mut display_text = accumulated.clone();
                if !got_done && !got_segment_break && commentary_text.is_none() {
                    display_text.push_str(&self.config.cursor);
                }
                let display = Self::clean_for_display(&display_text);

                if edit_supported && flood_strikes < max_flood_strikes {
                    if let Some(ref msg_id) = message_id {
                        match self.adapter.stream_edit(&self.chat_id, msg_id, &display).await {
                            Ok(ok) if ok => {
                                last_edit_time = Instant::now();
                                last_sent_text = display.clone();
                                current_update_visible = true;
                                flood_strikes = 0;
                            }
                            _ => {
                                // Edit failed — disable progressive editing
                                edit_supported = false;
                                flood_strikes += 1;
                            }
                        }
                    } else {
                        // First send
                        match self.adapter.stream_send(&self.chat_id, &display).await {
                            Ok(msg_id) => {
                                message_id = Some(msg_id);
                                self.already_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                                last_sent_text = display.clone();
                                last_edit_time = Instant::now();
                                current_update_visible = true;
                            }
                            Err(e) => {
                                warn!("Stream send failed: {e}");
                                edit_supported = false;
                            }
                        }
                    }
                } else if !edit_supported || flood_strikes >= max_flood_strikes {
                    // Fallback: send as new message
                    if message_id.is_none() || !self.already_sent() {
                        let display = Self::clean_for_display(&accumulated);
                        if !display.trim().is_empty() {
                            if let Ok(msg_id) = self.adapter.stream_send(&self.chat_id, &display).await {
                                message_id = Some(msg_id);
                                self.already_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                                current_update_visible = true;
                            }
                        }
                    }
                }
            }

            if got_done {
                // Final edit without cursor
                if !accumulated.is_empty() && edit_supported {
                    let display = Self::clean_for_display(&accumulated);
                    if let Some(ref msg_id) = message_id {
                        let _ = self.adapter.stream_edit(&self.chat_id, msg_id, &display).await;
                    } else if !self.already_sent() {
                        let _ = self.adapter.stream_send(&self.chat_id, &display).await;
                    }
                    if current_update_visible || message_id.is_some() {
                        self.final_response_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                return;
            }

            if let Some(commentary) = commentary_text {
                // Send commentary as a separate message
                last_sent_text.clear();
                let display = Self::clean_for_display(&commentary);
                if !display.trim().is_empty() {
                    if let Ok(_msg_id) = self.adapter.stream_send(&self.chat_id, &display).await {
                        self.already_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                last_edit_time = Instant::now();
                message_id = None;
                last_sent_text.clear();
            }

            // Tool boundary: reset message state so the next text chunk
            // creates a fresh message below any tool-progress messages
            if got_segment_break {
                message_id = None;
                last_sent_text.clear();
            }

            // Wait for more data
            if rx.recv().await.is_none() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    struct MockAdapter {
        max_len: usize,
    }

    #[async_trait]
    impl StreamEditTransport for MockAdapter {
        async fn stream_send(&self, _chat_id: &str, content: &str) -> Result<String, String> {
            Ok(format!("msg_{}", content.len()))
        }

        async fn stream_edit(&self, _chat_id: &str, _message_id: &str, _content: &str) -> Result<bool, String> {
            Ok(true)
        }

        fn max_message_length(&self) -> usize {
            self.max_len
        }
    }

    #[test]
    fn test_clean_for_display() {
        let input = "Hello [[audio_as_voice]] world MEDIA:/tmp/audio.mp3 done";
        let cleaned = GatewayStreamConsumer::clean_for_display(input);
        assert!(!cleaned.contains("[[audio_as_voice]]"));
        assert!(!cleaned.contains("MEDIA:"));
    }

    #[test]
    fn test_split_text_chunks_short() {
        let chunks = GatewayStreamConsumer::split_text_chunks("short text", 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "short text");
    }

    #[test]
    fn test_split_text_chunks_long() {
        let text = "A".repeat(3000);
        let chunks = GatewayStreamConsumer::split_text_chunks(&text, 1000);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.len() <= 1010); // Allow some slack
        }
    }

    #[test]
    fn test_split_text_chunks_respects_newlines() {
        let text = "first line\nsecond line\nthird line";
        let chunks = GatewayStreamConsumer::split_text_chunks(text, 20);
        // Should split at newline boundaries when possible
        assert!(chunks.len() >= 2);
    }
}
