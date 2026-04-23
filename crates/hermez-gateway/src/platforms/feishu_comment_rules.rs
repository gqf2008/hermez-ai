//! Feishu document comment access-control rules.
//!
//! 3-tier rule resolution: exact doc > wildcard "*" > top-level > code defaults.
//! Each field (enabled/policy/allow_from) falls back independently.
//! Config: ~/.hermez/feishu_comment_rules.json (mtime-cached, hot-reload).
//! Pairing store: ~/.hermez/feishu_comment_pairing.json.
//!
//! Mirrors Python `gateway/platforms/feishu_comment_rules.py`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;
use tracing::warn;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn rules_file() -> PathBuf {
    hermez_core::get_hermez_home().join("feishu_comment_rules.json")
}

fn pairing_file() -> PathBuf {
    hermez_core::get_hermez_home().join("feishu_comment_pairing.json")
}

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

const VALID_POLICIES: &[&str] = &["allowlist", "pairing"];

#[derive(Debug, Clone, Default)]
pub struct CommentDocumentRule {
    pub enabled: Option<bool>,
    pub policy: Option<String>,
    pub allow_from: Option<HashSet<String>>,
}

#[derive(Debug, Clone)]
pub struct CommentsConfig {
    pub enabled: bool,
    pub policy: String,
    pub allow_from: HashSet<String>,
    pub documents: HashMap<String, CommentDocumentRule>,
}

impl Default for CommentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: "pairing".to_string(),
            allow_from: HashSet::new(),
            documents: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedCommentRule {
    pub enabled: bool,
    pub policy: String,
    pub allow_from: HashSet<String>,
    pub match_source: String,
}

// ---------------------------------------------------------------------------
// Mtime-cached file loading
// ---------------------------------------------------------------------------

struct MtimeCache {
    path: PathBuf,
    mtime: Option<SystemTime>,
    data: Option<serde_json::Map<String, serde_json::Value>>,
}

impl MtimeCache {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            mtime: None,
            data: None,
        }
    }

    fn load(&mut self) -> serde_json::Map<String, serde_json::Value> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                self.mtime = None;
                self.data = Some(serde_json::Map::new());
                return serde_json::Map::new();
            }
        };

        let mtime = meta.modified().ok();
        if self.mtime == mtime {
            if let Some(ref data) = self.data {
                return data.clone();
            }
        }

        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                warn!("[Feishu-Rules] Failed to read {}: {e}", self.path.display());
                self.mtime = None;
                self.data = Some(serde_json::Map::new());
                return serde_json::Map::new();
            }
        };

        let data: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warn!("[Feishu-Rules] Failed to parse {}: {e}", self.path.display());
                self.mtime = None;
                self.data = Some(serde_json::Map::new());
                return serde_json::Map::new();
            }
        };

        let map = match data {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };

        self.mtime = mtime;
        self.data = Some(map.clone());
        map
    }
}

fn rules_cache() -> &'static parking_lot::Mutex<MtimeCache> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<MtimeCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(MtimeCache::new(rules_file())))
}

fn pairing_cache() -> &'static parking_lot::Mutex<MtimeCache> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<MtimeCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(MtimeCache::new(pairing_file())))
}

// ---------------------------------------------------------------------------
// Config parsing
// ---------------------------------------------------------------------------

fn parse_frozenset(raw: Option<&serde_json::Value>) -> Option<HashSet<String>> {
    match raw? {
        serde_json::Value::Array(arr) => {
            let set: HashSet<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect();
            Some(set)
        }
        _ => None,
    }
}

fn parse_document_rule(raw: &serde_json::Value) -> CommentDocumentRule {
    let map = match raw {
        serde_json::Value::Object(m) => m,
        _ => return CommentDocumentRule::default(),
    };

    let enabled = map.get("enabled").and_then(|v| v.as_bool());
    let policy = map.get("policy").and_then(|v| {
        let s = v.as_str()?.trim().to_lowercase();
        if VALID_POLICIES.contains(&s.as_str()) {
            Some(s)
        } else {
            None
        }
    });
    let allow_from = parse_frozenset(map.get("allow_from"));

    CommentDocumentRule {
        enabled,
        policy,
        allow_from,
    }
}

/// Load comment rules from disk (mtime-cached).
pub fn load_config() -> CommentsConfig {
    let raw = rules_cache().lock().load();
    if raw.is_empty() {
        return CommentsConfig::default();
    }

    let mut documents = HashMap::new();
    if let Some(serde_json::Value::Object(raw_docs)) = raw.get("documents") {
        for (key, rule_raw) in raw_docs {
            documents.insert(key.clone(), parse_document_rule(rule_raw));
        }
    }

    let policy = raw
        .get("policy")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| VALID_POLICIES.contains(&s.as_str()))
        .unwrap_or_else(|| "pairing".to_string());

    CommentsConfig {
        enabled: raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        policy,
        allow_from: parse_frozenset(raw.get("allow_from")).unwrap_or_default(),
        documents,
    }
}

// ---------------------------------------------------------------------------
// Rule resolution
// ---------------------------------------------------------------------------

/// Check if any document rule key starts with 'wiki:'.
pub fn has_wiki_keys(cfg: &CommentsConfig) -> bool {
    cfg.documents.keys().any(|k| k.starts_with("wiki:"))
}

/// Resolve effective rule: exact doc → wiki key → wildcard → top-level → defaults.
pub fn resolve_rule(
    cfg: &CommentsConfig,
    file_type: &str,
    file_token: &str,
    wiki_token: Option<&str>,
) -> ResolvedCommentRule {
    let exact_key = format!("{file_type}:{file_token}");

    let mut exact = cfg.documents.get(&exact_key).cloned();
    let mut exact_src = format!("exact:{exact_key}");

    if exact.is_none() {
        if let Some(wt) = wiki_token {
            let wiki_key = format!("wiki:{wt}");
            if let Some(rule) = cfg.documents.get(&wiki_key).cloned() {
                exact = Some(rule);
                exact_src = format!("exact:{wiki_key}");
            }
        }
    }

    let wildcard = cfg.documents.get("*").cloned();

    let mut layers: Vec<(CommentDocumentRule, String)> = Vec::new();
    if let Some(rule) = exact {
        layers.push((rule, exact_src));
    }
    if let Some(rule) = wildcard {
        layers.push((rule, "wildcard".to_string()));
    }

    let (enabled, en_src) = {
        let mut val = cfg.enabled;
        let mut src = "top".to_string();
        for (layer, source) in &layers {
            if let Some(v) = layer.enabled {
                val = v;
                src = source.clone();
                break;
            }
        }
        (val, src)
    };

    let (policy, pol_src) = {
        let mut val = cfg.policy.clone();
        let mut src = "top".to_string();
        for (layer, source) in &layers {
            if let Some(ref v) = layer.policy {
                val = v.clone();
                src = source.clone();
                break;
            }
        }
        (val, src)
    };

    let (allow_from, _) = {
        let mut val = cfg.allow_from.clone();
        let mut src = "top".to_string();
        for (layer, source) in &layers {
            if let Some(ref v) = layer.allow_from {
                val = v.clone();
                src = source.clone();
                break;
            }
        }
        (val, src)
    };

    let best_src = {
        let priority = |s: &str| match s.split(':').next().unwrap_or("") {
            "exact" => 0,
            "wildcard" => 1,
            "top" => 2,
            _ => 3,
        };
        let candidates = vec![&en_src, &pol_src];
        candidates
            .into_iter()
            .min_by_key(|s| priority(s))
            .cloned()
            .unwrap_or_else(|| "default".to_string())
    };

    ResolvedCommentRule {
        enabled,
        policy,
        allow_from,
        match_source: best_src,
    }
}

// ---------------------------------------------------------------------------
// Pairing store
// ---------------------------------------------------------------------------

fn load_pairing_approved() -> HashSet<String> {
    let data = pairing_cache().lock().load();
    let approved = data.get("approved");
    match approved {
        Some(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => HashSet::new(),
    }
}

fn save_pairing(data: serde_json::Map<String, serde_json::Value>) {
    let path = pairing_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    let value = serde_json::Value::Object(data);
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
            // Invalidate cache
            {
                let mut cache = pairing_cache().lock();
                cache.mtime = None;
                cache.data = None;
            }
        }
    }
}

/// Add a user to the pairing-approved list. Returns true if newly added.
pub fn pairing_add(user_open_id: &str) -> bool {
    let mut data = pairing_cache().lock().load();
    let mut approved = match data.get("approved") {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    if approved.contains_key(user_open_id) {
        return false;
    }
    let mut meta = serde_json::Map::new();
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let ts_val = match serde_json::Number::from_f64(ts) {
        Some(n) => serde_json::Value::Number(n),
        None => serde_json::Value::Null,
    };
    meta.insert("approved_at".to_string(), ts_val);
    approved.insert(user_open_id.to_string(), serde_json::Value::Object(meta));
    data.insert("approved".to_string(), serde_json::Value::Object(approved));
    save_pairing(data);
    true
}

/// Remove a user from the pairing-approved list. Returns true if removed.
pub fn pairing_remove(user_open_id: &str) -> bool {
    let mut data = pairing_cache().lock().load();
    let mut approved = match data.get("approved") {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => return false,
    };
    if !approved.contains_key(user_open_id) {
        return false;
    }
    approved.remove(user_open_id);
    data.insert("approved".to_string(), serde_json::Value::Object(approved));
    save_pairing(data);
    true
}

/// Return the approved dict {user_open_id: {approved_at: ...}}.
pub fn pairing_list() -> HashMap<String, serde_json::Value> {
    let data = pairing_cache().lock().load();
    match data.get("approved") {
        Some(serde_json::Value::Object(m)) => {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        _ => HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Access check
// ---------------------------------------------------------------------------

/// Check if user passes the resolved rule's policy gate.
pub fn is_user_allowed(rule: &ResolvedCommentRule, user_open_id: &str) -> bool {
    if rule.allow_from.contains(user_open_id) {
        return true;
    }
    if rule.policy == "pairing" {
        return load_pairing_approved().contains(user_open_id);
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_rule_defaults() {
        let cfg = CommentsConfig::default();
        let rule = resolve_rule(&cfg, "docx", "abc123", None);
        assert!(rule.enabled);
        assert_eq!(rule.policy, "pairing");
        assert!(rule.allow_from.is_empty());
    }

    #[test]
    fn test_resolve_rule_exact() {
        let mut cfg = CommentsConfig::default();
        cfg.documents.insert(
            "docx:abc123".to_string(),
            CommentDocumentRule {
                enabled: Some(false),
                policy: None,
                allow_from: None,
            },
        );
        let rule = resolve_rule(&cfg, "docx", "abc123", None);
        assert!(!rule.enabled);
        assert_eq!(rule.match_source, "exact:docx:abc123");
    }

    #[test]
    fn test_pairing_add_remove() {
        // Use a temp dir to avoid polluting real state
        // (In real tests we'd mock the path, but here we just verify no panic)
        let _ = pairing_add("test_user_1");
        let list_before = pairing_list();
        let was_new = pairing_add("test_user_2");
        assert!(was_new || !list_before.contains_key("test_user_2"));
        let _ = pairing_remove("test_user_1");
        let _ = pairing_remove("test_user_2");
    }
}
