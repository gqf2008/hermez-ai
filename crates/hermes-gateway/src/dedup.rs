#![allow(dead_code)]
//! TTL-based message deduplication cache.
//!
//! Used by gateway platform adapters (Feishu, WeCom, Dingtalk) to prevent
//! processing the same inbound message twice.
//!
//! Mirrors Python `MessageDeduplicator` in `gateway/platforms/helpers.py`
//! (commit 2edbf155).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// TTL-based deduplication cache for inbound messages.
pub struct MessageDeduplicator {
    entries: parking_lot::Mutex<HashMap<String, Instant>>,
    max_size: usize,
    ttl: Duration,
}

impl MessageDeduplicator {
    /// Create a new deduplicator with a 5-minute TTL and 2000-entry max.
    pub fn new() -> Self {
        Self::with_params(300, 2000)
    }

    /// Create with custom TTL (seconds) and max entries.
    pub fn with_params(ttl_secs: u64, max_size: usize) -> Self {
        Self {
            entries: parking_lot::Mutex::new(HashMap::with_capacity(max_size)),
            max_size,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Check if key was already seen within the TTL window.
    /// Expired entries are removed and treated as new.
    pub fn is_duplicate(&self, key: &str) -> bool {
        let mut map = self.entries.lock();
        let now = Instant::now();
        if let Some(ts) = map.get(key) {
            if now.duration_since(*ts) < self.ttl {
                return true;
            }
            // Entry has expired — remove it and treat as new
            map.remove(key);
            return false;
        }
        false
    }

    /// Record a message ID as seen.
    pub fn insert(&self, key: String) {
        let mut map = self.entries.lock();
        let now = Instant::now();
        // Purge expired entries
        map.retain(|_, ts| now.duration_since(*ts) < self.ttl);
        // LRU: evict if over max
        while map.len() >= self.max_size {
            if let Some(oldest) = map.iter().next().map(|(k, _)| k.clone()) {
                map.remove(&oldest);
            }
        }
        map.insert(key, now);
    }

    /// Clear all tracked messages.
    pub fn clear(&self) {
        self.entries.lock().clear();
    }
}

impl Default for MessageDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_dedup() {
        let dedup = MessageDeduplicator::new();
        assert!(!dedup.is_duplicate("msg1"));
        dedup.insert("msg1".to_string());
        assert!(dedup.is_duplicate("msg1"));
        assert!(!dedup.is_duplicate("msg2"));
    }

    #[test]
    fn test_empty_key_not_duplicate() {
        let dedup = MessageDeduplicator::new();
        assert!(!dedup.is_duplicate(""));
    }

    #[test]
    fn test_max_size_eviction() {
        let dedup = MessageDeduplicator::with_params(300, 2);
        dedup.insert("a".to_string());
        dedup.insert("b".to_string());
        dedup.insert("c".to_string()); // should evict one of a or b
        assert!(dedup.is_duplicate("c"));
        let remaining = dedup.is_duplicate("a") as usize + dedup.is_duplicate("b") as usize;
        assert_eq!(remaining, 1); // exactly one of a/b remains
    }

    #[test]
    fn test_clear() {
        let dedup = MessageDeduplicator::new();
        dedup.insert("msg1".to_string());
        dedup.clear();
        assert!(!dedup.is_duplicate("msg1"));
    }
}
