//! Feishu/Lark drive document comment handling.
//!
//! Processes `drive.notice.comment_add_v1` events and interacts with the
//! Drive v2 comment reaction API.  Kept in a separate module so that the
//! main `feishu.rs` adapter does not grow further and comment-related
//! logic can evolve independently.
//!
//! Flow:
//!   1. Parse event -> extract file_token, comment_id, reply_id, etc.
//!   2. Add OK reaction
//!   3. Parallel fetch: doc meta + comment details (batch_query)
//!   4. Branch on is_whole:
//!      Whole -> list whole comments timeline
//!      Local -> list comment thread replies
//!   5. Build prompt (local or whole)
//!   6. Run agent -> generate reply
//!   7. Route reply:
//!      Whole -> add_whole_comment
//!      Local -> reply_to_comment (fallback to add_whole_comment on 1069302)

use crate::utils::truncate_text_with_suffix;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

// ── Constants ──────────────────────────────────────────────────────────────

const REACTION_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v2/files/{file_token}/comments/reaction";
const BATCH_QUERY_META_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/metas/batch_query";
const BATCH_QUERY_COMMENT_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/files/{file_token}/comments/batch_query";
const LIST_COMMENTS_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/files/{file_token}/comments";
const LIST_REPLIES_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies";
const REPLY_COMMENT_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies";
const ADD_COMMENT_URI: &str =
    "https://open.feishu.cn/open-apis/drive/v1/files/{file_token}/new_comments";
const WIKI_GET_NODE_URI: &str =
    "https://open.feishu.cn/open-apis/wiki/v2/spaces/get_node";

const COMMENT_RETRY_LIMIT: usize = 6;
const COMMENT_RETRY_DELAY_S: f64 = 1.0;
const REPLY_CHUNK_SIZE: usize = 4000;
const PROMPT_TEXT_LIMIT: usize = 220;
const LOCAL_TIMELINE_LIMIT: usize = 20;
const WHOLE_TIMELINE_LIMIT: usize = 12;
const NO_REPLY_SENTINEL: &str = "NO_REPLY";

const ALLOWED_NOTICE_TYPES: &[&str] = &["add_comment", "add_reply"];

// Matches feishu/lark document URLs and extracts doc_type + token
static FEISHU_DOC_URL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?:feishu\.cn|larkoffice\.com|larksuite\.com|lark\.suite\.com)\
             /(?P<doc_type>wiki|doc|docx|sheet|sheets|slides|mindnote|bitable|base|file)\
             /(?P<token>[A-Za-z0-9_-]{10,40})"
        )
        .unwrap()
    });

// ── Data structures ────────────────────────────────────────────────────────

/// Parsed drive comment event.
#[derive(Debug, Clone, Default)]
pub struct DriveCommentEvent {
    pub event_id: String,
    pub comment_id: String,
    pub reply_id: String,
    pub is_mentioned: bool,
    pub timestamp: String,
    pub file_token: String,
    pub file_type: String,
    pub notice_type: String,
    pub from_open_id: String,
    pub to_open_id: String,
}

// ── HTTP helpers ───────────────────────────────────────────────────────────

async fn _exec_request(
    client: &Client,
    token: &str,
    method: reqwest::Method,
    uri: &str,
    body: Option<Value>,
) -> Result<(i64, String, Value), String> {
    let mut req = client
        .request(method.clone(), uri)
        .header("Authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {e}"))?;

    let code = resp_body
        .get("code")
        .and_then(|v| v.as_i64())
        .unwrap_or(if status.is_success() { 0 } else { -1 });
    let msg = resp_body
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data = resp_body.get("data").cloned().unwrap_or(Value::Null);

    debug!(
        "[Feishu-Comment] API {} {} -> code={} msg={} data_keys={:?}",
        method,
        uri,
        code,
        msg,
        if let serde_json::Value::Object(m) = &data {
            m.keys().cloned().collect::<Vec<_>>()
        } else {
            vec![]
        }
    );

    Ok((code, msg, data))
}

// ── Event parsing ──────────────────────────────────────────────────────────

/// Extract structured fields from a `drive.notice.comment_add_v1` payload.
pub fn parse_drive_comment_event(data: &Value) -> Option<DriveCommentEvent> {
    let event = data.get("event")?;

    let notice_meta = event.get("notice_meta").and_then(|v| v.as_object());
    let from_user = notice_meta
        .and_then(|m| m.get("from_user_id"))
        .and_then(|v| v.as_object());
    let to_user = notice_meta
        .and_then(|m| m.get("to_user_id"))
        .and_then(|v| v.as_object());

    Some(DriveCommentEvent {
        event_id: event.get("event_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        comment_id: event.get("comment_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        reply_id: event.get("reply_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        is_mentioned: event.get("is_mentioned").and_then(|v| v.as_bool()).unwrap_or(false),
        timestamp: event.get("timestamp").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        file_token: notice_meta
            .and_then(|m| m.get("file_token"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        file_type: notice_meta
            .and_then(|m| m.get("file_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        notice_type: notice_meta
            .and_then(|m| m.get("notice_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        from_open_id: from_user
            .and_then(|m| m.get("open_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        to_open_id: to_user
            .and_then(|m| m.get("open_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

// ── Comment reaction API ───────────────────────────────────────────────────

/// Add an emoji reaction to a document comment reply.
pub async fn add_comment_reaction(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    reply_id: &str,
    reaction_type: &str,
) -> bool {
    let uri = REACTION_URI.replace("{file_token}", file_token);
    let body = serde_json::json!({
        "action": "add",
        "reply_id": reply_id,
        "reaction_type": reaction_type,
    });

    match _exec_request(client, token, reqwest::Method::POST, &uri, Some(body)).await {
        Ok((code, _, _)) => {
            let ok = code == 0;
            if ok {
                info!("[Feishu-Comment] Reaction '{reaction_type}' added: file={file_type}:{file_token} reply={reply_id}");
            } else {
                warn!("[Feishu-Comment] Reaction API failed: code={code} file={file_type}:{file_token} reply={reply_id}");
            }
            ok
        }
        Err(e) => {
            warn!("[Feishu-Comment] Reaction request error: {e}");
            false
        }
    }
}

/// Remove an emoji reaction from a document comment reply.
pub async fn delete_comment_reaction(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    reply_id: &str,
    reaction_type: &str,
) -> bool {
    let uri = REACTION_URI.replace("{file_token}", file_token);
    let body = serde_json::json!({
        "action": "delete",
        "reply_id": reply_id,
        "reaction_type": reaction_type,
    });

    match _exec_request(client, token, reqwest::Method::POST, &uri, Some(body)).await {
        Ok((code, _, _)) => {
            let ok = code == 0;
            if ok {
                info!("[Feishu-Comment] Reaction '{reaction_type}' deleted: file={file_type}:{file_token} reply={reply_id}");
            } else {
                warn!("[Feishu-Comment] Reaction delete failed: code={code}");
            }
            ok
        }
        Err(e) => {
            warn!("[Feishu-Comment] Reaction delete error: {e}");
            false
        }
    }
}

// ── API call layer ─────────────────────────────────────────────────────────

/// Fetch document title and URL via batch_query meta API.
pub async fn query_document_meta(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
) -> HashMap<String, String> {
    let body = serde_json::json!({
        "request_docs": [{"doc_token": file_token, "doc_type": file_type}],
        "with_url": true,
    });

    match _exec_request(
        client,
        token,
        reqwest::Method::POST,
        BATCH_QUERY_META_URI,
        Some(body),
    )
    .await
    {
        Ok((0, _, data)) => {
            let mut result = HashMap::new();
            let meta = data
                .get("metas")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .or_else(|| {
                    data.get("metas")
                        .and_then(|v| v.as_object())
                        .and_then(|m| m.get(file_token))
                })
                .and_then(|v| v.as_object());

            if let Some(m) = meta {
                result.insert(
                    "title".to_string(),
                    m.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                );
                result.insert(
                    "url".to_string(),
                    m.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                );
                result.insert(
                    "doc_type".to_string(),
                    m.get("doc_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or(file_type)
                        .to_string(),
                );
            }
            result
        }
        Ok((code, msg, _)) => {
            warn!("[Feishu-Comment] Meta batch_query failed: code={code} msg={msg}");
            HashMap::new()
        }
        Err(e) => {
            warn!("[Feishu-Comment] Meta batch_query error: {e}");
            HashMap::new()
        }
    }
}

/// Fetch comment details via batch_query comment API.
pub async fn batch_query_comment(
    client: &Client,
    token: &str,
    file_token: &str,
    _file_type: &str,
    comment_id: &str,
) -> Value {
    for attempt in 0..COMMENT_RETRY_LIMIT {
        let uri = BATCH_QUERY_COMMENT_URI.replace("{file_token}", file_token);
        let body = serde_json::json!({
            "comment_ids": [comment_id],
        });

        match _exec_request(
            client,
            token,
            reqwest::Method::POST,
            &uri,
            Some(body),
        )
        .await
        {
            Ok((0, _, data)) => {
                if let Some(items) = data.get("items").and_then(|v| v.as_array()) {
                    if let Some(first) = items.first() {
                        return first.clone();
                    }
                }
                warn!("[Feishu-Comment] batch_query_comment: empty items");
                return Value::Null;
            }
            Ok((code, msg, _)) => {
                if attempt < COMMENT_RETRY_LIMIT - 1 {
                    info!(
                        "[Feishu-Comment] batch_query_comment retry {}/{}: code={code} msg={msg}",
                        attempt + 1,
                        COMMENT_RETRY_LIMIT
                    );
                    tokio::time::sleep(Duration::from_secs_f64(COMMENT_RETRY_DELAY_S)).await;
                } else {
                    warn!(
                        "[Feishu-Comment] batch_query_comment failed after {} attempts: code={code} msg={msg}",
                        COMMENT_RETRY_LIMIT,
                    );
                    return Value::Null;
                }
            }
            Err(e) => {
                if attempt < COMMENT_RETRY_LIMIT - 1 {
                    info!(
                        "[Feishu-Comment] batch_query_comment retry {}/{}: {e}",
                        attempt + 1,
                        COMMENT_RETRY_LIMIT
                    );
                    tokio::time::sleep(Duration::from_secs_f64(COMMENT_RETRY_DELAY_S)).await;
                } else {
                    warn!(
                        "[Feishu-Comment] batch_query_comment failed after {} attempts: {e}",
                        COMMENT_RETRY_LIMIT,
                    );
                    return Value::Null;
                }
            }
        }
    }
    Value::Null
}

/// List all whole-document comments (paginated, up to 500).
pub async fn list_whole_comments(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
) -> Vec<Value> {
    let mut all_comments = Vec::new();
    let mut page_token = String::new();

    for _ in 0..5 {
        let mut uri = format!(
            "{}?file_type={}&is_whole=true&page_size=100&user_id_type=open_id",
            LIST_COMMENTS_URI.replace("{file_token}", file_token),
            urlencoding::encode(file_type)
        );
        if !page_token.is_empty() {
            uri.push_str(&format!("&page_token={}", urlencoding::encode(&page_token)));
        }

        match _exec_request(client, token, reqwest::Method::GET, &uri, None).await {
            Ok((0, _, data)) => {
                if let Some(items) = data.get("items").and_then(|v| v.as_array()) {
                    all_comments.extend(items.iter().cloned());
                }
                if !data.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false) {
                    break;
                }
                page_token = data
                    .get("page_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if page_token.is_empty() {
                    break;
                }
            }
            Ok((code, msg, _)) => {
                warn!(
                    "[Feishu-Comment] List whole comments failed: code={code} msg={msg}"
                );
                break;
            }
            Err(e) => {
                warn!("[Feishu-Comment] List whole comments error: {e}");
                break;
            }
        }
    }

    info!(
        "[Feishu-Comment] list_whole_comments: total {} whole comments fetched",
        all_comments.len()
    );
    all_comments
}

/// List all replies in a comment thread (paginated, up to 500).
pub async fn list_comment_replies(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    expect_reply_id: &str,
) -> Vec<Value> {
    for attempt in 0..COMMENT_RETRY_LIMIT {
        let mut all_replies = Vec::new();
        let mut page_token = String::new();
        let mut fetch_ok = true;

        for _ in 0..5 {
            let mut uri = format!(
                "{}?file_type={}&page_size=100&user_id_type=open_id",
                LIST_REPLIES_URI
                    .replace("{file_token}", file_token)
                    .replace("{comment_id}", comment_id),
                urlencoding::encode(file_type)
            );
            if !page_token.is_empty() {
                uri.push_str(&format!("&page_token={}", urlencoding::encode(&page_token)));
            }

            match _exec_request(client, token, reqwest::Method::GET, &uri, None).await {
                Ok((0, _, data)) => {
                    if let Some(items) = data.get("items").and_then(|v| v.as_array()) {
                        all_replies.extend(items.iter().cloned());
                    }
                    if !data.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false) {
                        break;
                    }
                    page_token = data
                        .get("page_token")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if page_token.is_empty() {
                        break;
                    }
                }
                Ok((code, msg, _)) => {
                    warn!(
                        "[Feishu-Comment] List replies failed: code={code} msg={msg}"
                    );
                    fetch_ok = false;
                    break;
                }
                Err(e) => {
                    warn!("[Feishu-Comment] List replies error: {e}");
                    fetch_ok = false;
                    break;
                }
            }
        }

        if expect_reply_id.is_empty() || !fetch_ok {
            return all_replies;
        }
        let found = all_replies.iter().any(|r| {
            r.get("reply_id")
                .and_then(|v| v.as_str())
                .map(|id| id == expect_reply_id)
                .unwrap_or(false)
        });
        if found {
            return all_replies;
        }
        if attempt < COMMENT_RETRY_LIMIT - 1 {
            info!(
                "[Feishu-Comment] list_comment_replies: reply_id={expect_reply_id} not found, retry {}/{}",
                attempt + 1,
                COMMENT_RETRY_LIMIT
            );
            tokio::time::sleep(Duration::from_secs_f64(COMMENT_RETRY_DELAY_S)).await;
        } else {
            warn!(
                "[Feishu-Comment] list_comment_replies: reply_id={expect_reply_id} not found after {} attempts",
                COMMENT_RETRY_LIMIT
            );
            return all_replies;
        }
    }

    Vec::new()
}

// ── Reply helpers ──────────────────────────────────────────────────────────

/// Escape characters not allowed in Feishu comment text_run content.
fn _sanitize_comment_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Post a reply to a local comment thread.
pub async fn reply_to_comment(
    client: &Client,
    token: &str,
    file_token: &str,
    _file_type: &str,
    comment_id: &str,
    text: &str,
) -> (bool, i64) {
    let text = _sanitize_comment_text(text);
    info!("[Feishu-Comment] reply_to_comment: comment_id={comment_id} text={}", &text[..text.len().min(100)]);

    let uri = REPLY_COMMENT_URI
        .replace("{file_token}", file_token)
        .replace("{comment_id}", comment_id);
    let body = serde_json::json!({
        "content": {
            "elements": [
                {"type": "text_run", "text_run": {"text": text}},
            ]
        }
    });

    match _exec_request(client, token, reqwest::Method::POST, &uri, Some(body)).await {
        Ok((code, msg, _)) => {
            if code != 0 {
                warn!("[Feishu-Comment] reply_to_comment FAILED: code={code} msg={msg} comment_id={comment_id}");
            } else {
                info!("[Feishu-Comment] reply_to_comment OK: comment_id={comment_id}");
            }
            (code == 0, code)
        }
        Err(e) => {
            warn!("[Feishu-Comment] reply_to_comment error: {e}");
            (false, -1)
        }
    }
}

/// Add a new whole-document comment.
pub async fn add_whole_comment(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    text: &str,
) -> bool {
    let text = _sanitize_comment_text(text);
    info!("[Feishu-Comment] add_whole_comment: file_token={file_token} text={}", &text[..text.len().min(100)]);

    let uri = ADD_COMMENT_URI.replace("{file_token}", file_token);
    let body = serde_json::json!({
        "file_type": file_type,
        "reply_elements": [
            {"type": "text", "text": text},
        ],
    });

    match _exec_request(client, token, reqwest::Method::POST, &uri, Some(body)).await {
        Ok((code, msg, _)) => {
            if code != 0 {
                warn!("[Feishu-Comment] add_whole_comment FAILED: code={code} msg={msg}");
            } else {
                info!("[Feishu-Comment] add_whole_comment OK");
            }
            code == 0
        }
        Err(e) => {
            warn!("[Feishu-Comment] add_whole_comment error: {e}");
            false
        }
    }
}

/// Split text into chunks for delivery, preferring line breaks.
fn _chunk_text(text: &str, limit: usize) -> Vec<&str> {
    if text.len() <= limit {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= limit {
            chunks.push(remaining);
            break;
        }
        // Find last newline within limit
        let cut = remaining[..limit].rfind('\n').unwrap_or(limit);
        let cut = if cut == 0 { limit } else { cut };
        chunks.push(&remaining[..cut]);
        remaining = remaining[cut..].trim_start_matches('\n');
    }
    chunks
}

/// Route agent reply to the correct API, chunking long text.
pub async fn deliver_comment_reply(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    text: &str,
    mut is_whole: bool,
) -> bool {
    let chunks = _chunk_text(text, REPLY_CHUNK_SIZE);
    info!(
        "[Feishu-Comment] deliver_comment_reply: is_whole={is_whole} comment_id={comment_id} text_len={} chunks={}",
        text.len(),
        chunks.len()
    );

    let mut all_ok = true;
    for (i, chunk) in chunks.iter().enumerate() {
        if chunks.len() > 1 {
            info!(
                "[Feishu-Comment] deliver_comment_reply: sending chunk {}/{} ({} chars)",
                i + 1,
                chunks.len(),
                chunk.len()
            );
        }

        let ok = if is_whole {
            add_whole_comment(client, token, file_token, file_type, chunk).await
        } else {
            let (success, code) = reply_to_comment(client, token, file_token, file_type, comment_id, chunk).await;
            if success {
                true
            } else if code == 1069302 {
                info!("[Feishu-Comment] Reply not allowed (1069302), falling back to add_whole_comment");
                let ok = add_whole_comment(client, token, file_token, file_type, chunk).await;
                is_whole = true; // subsequent chunks also use add_comment
                ok
            } else {
                false
            }
        };

        if !ok {
            all_ok = false;
            break;
        }
    }

    all_ok
}

// ── Comment content extraction helpers ─────────────────────────────────────

/// Extract plain text from a comment reply's content structure.
fn _extract_reply_text(reply: &Value) -> String {
    let content = reply.get("content").cloned().unwrap_or(Value::Null);
    let content = if let Value::String(s) = content {
        serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
    } else {
        content
    };

    let elements = content.get("elements").and_then(|v| v.as_array());
    let mut parts = Vec::new();
    if let Some(elems) = elements {
        for elem in elems {
            let ty = elem.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "text_run" => {
                    if let Some(text) = elem
                        .get("text_run")
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(text.to_string());
                    }
                }
                "docs_link" => {
                    if let Some(url) = elem
                        .get("docs_link")
                        .and_then(|v| v.get("url"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(url.to_string());
                    }
                }
                "person" => {
                    if let Some(uid) = elem
                        .get("person")
                        .and_then(|v| v.get("user_id"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(format!("@{uid}"));
                    }
                }
                _ => {}
            }
        }
    }
    parts.concat()
}

/// Extract user_id from a reply dict.
fn _get_reply_user_id(reply: &Value) -> String {
    let user_id = reply.get("user_id").cloned().unwrap_or(Value::Null);
    match user_id {
        Value::Object(m) => m
            .get("open_id")
            .or_else(|| m.get("user_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Value::String(s) => s,
        _ => user_id.to_string(),
    }
}

/// Extract semantic text from a reply, stripping self @mentions and extra whitespace.
fn _extract_semantic_text(reply: &Value, self_open_id: &str) -> String {
    let content = reply.get("content").cloned().unwrap_or(Value::Null);
    let content = if let Value::String(s) = content {
        serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
    } else {
        content
    };

    let elements = content.get("elements").and_then(|v| v.as_array());
    let mut parts = Vec::new();
    if let Some(elems) = elements {
        for elem in elems {
            let ty = elem.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "person" => {
                    if let Some(uid) = elem
                        .get("person")
                        .and_then(|v| v.get("user_id"))
                        .and_then(|v| v.as_str())
                    {
                        if !self_open_id.is_empty() && uid == self_open_id {
                            continue;
                        }
                        parts.push(format!("@{uid}"));
                    }
                }
                "text_run" => {
                    if let Some(text) = elem
                        .get("text_run")
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(text.to_string());
                    }
                }
                "docs_link" => {
                    if let Some(url) = elem
                        .get("docs_link")
                        .and_then(|v| v.get("url"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(url.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let text = parts.concat();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract unique document links from a list of comment replies.
fn _extract_docs_links(replies: &[Value]) -> Vec<HashMap<String, String>> {
    let mut seen_tokens = std::collections::HashSet::new();
    let mut links = Vec::new();

    for reply in replies {
        let content = reply.get("content").cloned().unwrap_or(Value::Null);
        let content = if let Value::String(s) = content {
            serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
        } else {
            content
        };

        let elements = content.get("elements").and_then(|v| v.as_array());
        if let Some(elems) = elements {
            for elem in elems {
                let ty = elem.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if !matches!(ty, "docs_link" | "link") {
                    continue;
                }
                let link_data = elem
                    .get("docs_link")
                    .or_else(|| elem.get("link"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let url = link_data
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() {
                    continue;
                }
                if let Some(caps) = FEISHU_DOC_URL_RE.captures(&url) {
                    let doc_type = caps
                        .name("doc_type")
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let token = caps
                        .name("token")
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    if token.is_empty() || seen_tokens.contains(&token) {
                        continue;
                    }
                    seen_tokens.insert(token.clone());
                    let mut link = HashMap::new();
                    link.insert("url".to_string(), url);
                    link.insert("doc_type".to_string(), doc_type);
                    link.insert("token".to_string(), token);
                    links.push(link);
                }
            }
        }
    }
    links
}

// ── Wiki resolution ────────────────────────────────────────────────────────

/// Reverse-lookup: given an obj_token, find its wiki node_token.
async fn _reverse_lookup_wiki_token(
    client: &Client,
    token: &str,
    obj_type: &str,
    obj_token: &str,
) -> Option<String> {
    let mut uri = format!("{}?token={}", WIKI_GET_NODE_URI, urlencoding::encode(obj_token));
    if !obj_type.is_empty() {
        uri.push_str(&format!("&obj_type={}", urlencoding::encode(obj_type)));
    }

    match _exec_request(client, token, reqwest::Method::GET, &uri, None).await {
        Ok((0, _, data)) => data
            .get("node")
            .and_then(|v| v.get("node_token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Ok((code, msg, _)) => {
            warn!(
                "[Feishu-Comment] Wiki reverse lookup failed: code={code} msg={msg} obj={obj_type}:{obj_token}"
            );
            None
        }
        Err(e) => {
            warn!("[Feishu-Comment] Wiki reverse lookup error: {e}");
            None
        }
    }
}

/// Resolve wiki links to their underlying document type and token.
async fn _resolve_wiki_nodes(
    client: &Client,
    token: &str,
    links: &mut [HashMap<String, String>],
) {
    for link in links.iter_mut() {
        if link.get("doc_type").map(|s| s.as_str()) != Some("wiki") {
            continue;
        }
        let wiki_token = link.get("token").cloned().unwrap_or_default();
        if wiki_token.is_empty() {
            continue;
        }

        let uri = format!(
            "{}?token={}",
            WIKI_GET_NODE_URI,
            urlencoding::encode(&wiki_token)
        );
        match _exec_request(client, token, reqwest::Method::GET, &uri, None).await {
            Ok((0, _, data)) => {
                if let Some(node) = data.get("node").and_then(|v| v.as_object()) {
                    let resolved_type = node
                        .get("obj_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let resolved_token = node
                        .get("obj_token")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !resolved_type.is_empty() && !resolved_token.is_empty() {
                        info!(
                            "[Feishu-Comment] Wiki resolved: {wiki_token} -> {resolved_type}:{resolved_token}"
                        );
                        link.insert("resolved_type".to_string(), resolved_type);
                        link.insert("resolved_token".to_string(), resolved_token);
                    } else {
                        warn!("[Feishu-Comment] Wiki resolve returned empty: {wiki_token}");
                    }
                }
            }
            Ok((code, msg, _)) => {
                warn!(
                    "[Feishu-Comment] Wiki resolve failed: code={code} msg={msg} token={wiki_token}"
                );
            }
            Err(e) => {
                warn!("[Feishu-Comment] Wiki resolve error: {e}");
            }
        }
    }
}

/// Format resolved document links for prompt embedding.
fn _format_referenced_docs(
    links: &[HashMap<String, String>],
    current_file_token: &str,
) -> String {
    if links.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        "Referenced documents in comments:".to_string(),
    ];
    for link in links {
        let rtype = link
            .get("resolved_type")
            .or_else(|| link.get("doc_type"))
            .cloned()
            .unwrap_or_default();
        let rtoken = link
            .get("resolved_token")
            .or_else(|| link.get("token"))
            .cloned()
            .unwrap_or_default();
        let is_current = rtoken == current_file_token;
        let suffix = if is_current { " (same as current document)" } else { "" };
        let url = link.get("url").cloned().unwrap_or_default();
        lines.push(format!("- {rtype}:{rtoken}{suffix} ({})", &url[..url.len().min(80)]));
    }
    lines.join("\n")
}

// ── Prompt construction ────────────────────────────────────────────────────

/// Select up to `LOCAL_TIMELINE_LIMIT` entries centered on target_index.
fn _select_local_timeline(
    timeline: &[(String, String, bool)],
    target_index: usize,
) -> Vec<(String, String, bool)> {
    let n = timeline.len();
    if n <= LOCAL_TIMELINE_LIMIT {
        return timeline.to_vec();
    }
    let mut selected = std::collections::HashSet::new();
    selected.insert(0usize);
    selected.insert(n - 1);
    if target_index < n {
        selected.insert(target_index);
    }
    let mut budget = LOCAL_TIMELINE_LIMIT.saturating_sub(selected.len());
    let (mut lo, mut hi) = (target_index.saturating_sub(1), target_index + 1);
    while budget > 0 && (lo > 0 || hi < n) {
        if lo > 0 && selected.insert(lo) {
            budget -= 1;
        }
        lo = lo.saturating_sub(1);
        if budget > 0 && hi < n && selected.insert(hi) {
            budget -= 1;
        }
        hi += 1;
    }
    let mut indices: Vec<_> = selected.into_iter().collect();
    indices.sort_unstable();
    indices.into_iter().map(|i| timeline[i].clone()).collect()
}

/// Select up to `WHOLE_TIMELINE_LIMIT` entries for whole-doc comments.
fn _select_whole_timeline(
    timeline: &[(String, String, bool)],
    current_index: usize,
    nearest_self_index: usize,
) -> Vec<(String, String, bool)> {
    let n = timeline.len();
    if n <= WHOLE_TIMELINE_LIMIT {
        return timeline.to_vec();
    }
    let mut selected = std::collections::HashSet::new();
    if current_index < n {
        selected.insert(current_index);
    }
    if nearest_self_index < n {
        selected.insert(nearest_self_index);
    }
    let mut budget = WHOLE_TIMELINE_LIMIT.saturating_sub(selected.len());
    let (mut lo, mut hi) = (current_index.saturating_sub(1), current_index + 1);
    while budget > 0 && (lo > 0 || hi < n) {
        if lo > 0 && selected.insert(lo) {
            budget -= 1;
        }
        lo = lo.saturating_sub(1);
        if budget > 0 && hi < n && selected.insert(hi) {
            budget -= 1;
        }
        hi += 1;
    }
    if selected.is_empty() {
        return timeline[n.saturating_sub(WHOLE_TIMELINE_LIMIT)..].to_vec();
    }
    let mut indices: Vec<_> = selected.into_iter().collect();
    indices.sort_unstable();
    indices.into_iter().map(|i| timeline[i].clone()).collect()
}

const COMMON_INSTRUCTIONS: &str = r#"This is a Feishu document comment thread, not an IM chat.
Do NOT call feishu_drive_add_comment or feishu_drive_reply_comment yourself.
Your reply will be posted automatically. Just output the reply text.
Use the thread timeline above as the main context.
If the quoted content is not enough, use feishu_doc_read to read nearby context.
The quoted content is your primary anchor — insert/summarize/explain requests are about it.
Do not guess document content you haven't read.
Reply in the same language as the user's comment unless they request otherwise.
Use plain text only. Do not use Markdown, headings, bullet lists, tables, or code blocks.
Do not show your reasoning process. Do not start with "I will", "Let me", or "I'll first".
Output only the final user-facing reply.
If no reply is needed, output exactly NO_REPLY."#;

/// Build the prompt for a local (quoted-text) comment.
pub fn build_local_comment_prompt(
    doc_title: &str,
    doc_url: &str,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    quote_text: &str,
    root_comment_text: &str,
    target_reply_text: &str,
    timeline: &[(String, String, bool)],
    _self_open_id: &str,
    target_index: usize,
    referenced_docs: &str,
) -> String {
    let selected = _select_local_timeline(timeline, target_index);
    let mut lines = vec![
        format!(r#"The user added a reply in "{doc_title}"."#),
        format!(r#"Current user comment text: "{}""#, truncate_text_with_suffix(target_reply_text, PROMPT_TEXT_LIMIT, "...")),
        format!(r#"Original comment text: "{}""#, truncate_text_with_suffix(root_comment_text, PROMPT_TEXT_LIMIT, "...")),
        format!(r#"Quoted content: "{}""#, truncate_text_with_suffix(quote_text, 500, "...")),
        "This comment mentioned you (@mention is for routing, not task content).".to_string(),
        format!("Document link: {doc_url}"),
        "Current commented document:".to_string(),
        format!("- file_type={file_type}"),
        format!("- file_token={file_token}"),
        format!("- comment_id={comment_id}"),
        String::new(),
        format!(
            "Current comment card timeline ({}/{} entries):",
            selected.len(),
            timeline.len()
        ),
    ];

    for (user_id, text, is_self) in &selected {
        let marker = if *is_self { " <-- YOU" } else { "" };
        lines.push(format!(
            "[{}] {}{marker}",
            user_id,
            truncate_text_with_suffix(text, PROMPT_TEXT_LIMIT, "...")
        ));
    }

    if !referenced_docs.is_empty() {
        lines.push(referenced_docs.to_string());
    }

    lines.push(String::new());
    lines.push(COMMON_INSTRUCTIONS.to_string());
    lines.join("\n")
}

/// Build the prompt for a whole-document comment.
pub fn build_whole_comment_prompt(
    doc_title: &str,
    doc_url: &str,
    file_token: &str,
    file_type: &str,
    comment_text: &str,
    timeline: &[(String, String, bool)],
    _self_open_id: &str,
    current_index: usize,
    nearest_self_index: usize,
    referenced_docs: &str,
) -> String {
    let selected = _select_whole_timeline(timeline, current_index, nearest_self_index);
    let mut lines = vec![
        format!(r#"The user added a comment in "{doc_title}"."#),
        format!(r#"Current user comment text: "{}""#, truncate_text_with_suffix(comment_text, PROMPT_TEXT_LIMIT, "...")),
        "This is a whole-document comment.".to_string(),
        "This comment mentioned you (@mention is for routing, not task content).".to_string(),
        format!("Document link: {doc_url}"),
        "Current commented document:".to_string(),
        format!("- file_type={file_type}"),
        format!("- file_token={file_token}"),
        String::new(),
        format!(
            "Whole-document comment timeline ({}/{} entries):",
            selected.len(),
            timeline.len()
        ),
    ];

    for (user_id, text, is_self) in &selected {
        let marker = if *is_self { " <-- YOU" } else { "" };
        lines.push(format!(
            "[{}] {}{marker}",
            user_id,
            truncate_text_with_suffix(text, PROMPT_TEXT_LIMIT, "...")
        ));
    }

    if !referenced_docs.is_empty() {
        lines.push(referenced_docs.to_string());
    }

    lines.push(String::new());
    lines.push(COMMON_INSTRUCTIONS.to_string());
    lines.join("\n")
}

// ── Session cache ──────────────────────────────────────────────────────────

const SESSION_MAX_MESSAGES: usize = 50;
const SESSION_TTL_S: u64 = 3600;

#[derive(Clone)]
struct SessionEntry {
    messages: Vec<HashMap<String, String>>,
    last_access: Instant,
}

static SESSION_CACHE: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, SessionEntry>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn _session_key(file_type: &str, file_token: &str) -> String {
    format!("comment-doc:{file_type}:{file_token}")
}

fn _load_session_history(key: &str) -> Vec<HashMap<String, String>> {
    let mut cache = SESSION_CACHE.lock();
    let entry = match cache.get(key) {
        Some(e) => e,
        None => return Vec::new(),
    };
    if Instant::now().duration_since(entry.last_access).as_secs() > SESSION_TTL_S {
        cache.remove(key);
        info!("[Feishu-Comment] Session expired: {key}");
        return Vec::new();
    }
    let mut entry = entry.clone();
    entry.last_access = Instant::now();
    cache.insert(key.to_string(), entry.clone());
    entry.messages
}

fn _save_session_history(key: &str, messages: &[HashMap<String, String>]) {
    let cleaned: Vec<_> = messages
        .iter()
        .filter(|m| {
            let role = m.get("role").map(|s| s.as_str()).unwrap_or("");
            (role == "user" || role == "assistant") && m.get("content").is_some()
        })
        .cloned()
        .collect();
    let trimmed = if cleaned.len() > SESSION_MAX_MESSAGES {
        &cleaned[cleaned.len().saturating_sub(SESSION_MAX_MESSAGES)..]
    } else {
        &cleaned[..]
    };
    let mut cache = SESSION_CACHE.lock();
    cache.insert(
        key.to_string(),
        SessionEntry {
            messages: trimmed.to_vec(),
            last_access: Instant::now(),
        },
    );
    info!(
        "[Feishu-Comment] Session saved: {key} ({} messages)",
        trimmed.len()
    );
}

// ── Agent execution (stub) ─────────────────────────────────────────────────

/// Run the comment agent with the given prompt.
/// Run the comment agent with an optional MessageHandler.
/// Falls back to a placeholder if no handler is available.
async fn _run_comment_agent(
    prompt: &str,
    message_handler: Option<&dyn crate::runner::MessageHandler>,
) -> String {
    if let Some(handler) = message_handler {
        match handler.run_with_prompt(prompt).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("[Feishu-Comment] Agent run_with_prompt failed: {e}");
                format!(
                    "[AI 助手处理出错: {e}]\n\n{}\n\n请稍后重试。",
                    prompt.chars().take(200).collect::<String>()
                )
            }
        }
    } else {
        info!("[Feishu-Comment] No MessageHandler available — returning placeholder");
        format!(
            "[AI 助手已收到文档评论请求，正在处理中...]\n\n{}\n\n请稍候，完整回复稍后送达。",
            prompt.chars().take(200).collect::<String>()
        )
    }
}

// ── Event handler entry point ──────────────────────────────────────────────

/// Full orchestration for a drive comment event.
pub async fn handle_drive_comment_event(
    client: &Client,
    token: &str,
    data: &Value,
    self_open_id: &str,
    message_handler: Option<&dyn crate::runner::MessageHandler>,
) {
    info!("[Feishu-Comment] ========== handle_drive_comment_event START ==========");
    let parsed = match parse_drive_comment_event(data) {
        Some(p) => p,
        None => {
            warn!("[Feishu-Comment] Dropping malformed drive comment event");
            return;
        }
    };
    info!("[Feishu-Comment] [Step 0/5] Event parsed successfully");

    let file_token = &parsed.file_token;
    let file_type = &parsed.file_type;
    let comment_id = &parsed.comment_id;
    let reply_id = &parsed.reply_id;
    let from_open_id = &parsed.from_open_id;
    let to_open_id = &parsed.to_open_id;
    let notice_type = &parsed.notice_type;

    // Filter: self-reply, receiver check, notice_type
    if !from_open_id.is_empty() && !self_open_id.is_empty() && from_open_id == self_open_id {
        debug!("[Feishu-Comment] Skipping self-authored event: from={from_open_id}");
        return;
    }
    if to_open_id.is_empty() || (!self_open_id.is_empty() && to_open_id != self_open_id) {
        debug!(
            "[Feishu-Comment] Skipping event not addressed to self: to={}",
            to_open_id
        );
        return;
    }
    if !notice_type.is_empty() && !ALLOWED_NOTICE_TYPES.contains(&notice_type.as_str()) {
        debug!("[Feishu-Comment] Skipping notice_type={notice_type}");
        return;
    }
    if file_token.is_empty() || file_type.is_empty() || comment_id.is_empty() {
        warn!("[Feishu-Comment] Missing required fields, skipping");
        return;
    }

    info!(
        "[Feishu-Comment] Event: notice={notice_type} file={file_type}:{file_token} comment={comment_id} from={from_open_id}"
    );

    // Access control
    let comments_cfg = crate::platforms::feishu_comment_rules::load_config();
    let mut rule = crate::platforms::feishu_comment_rules::resolve_rule(
        &comments_cfg,
        file_type,
        file_token,
        None,
    );

    if rule.match_source != "exact" && crate::platforms::feishu_comment_rules::has_wiki_keys(&comments_cfg) {
        if let Some(wiki_token) = _reverse_lookup_wiki_token(client, token, file_type, file_token).await {
            rule = crate::platforms::feishu_comment_rules::resolve_rule(
                &comments_cfg,
                file_type,
                file_token,
                Some(&wiki_token),
            );
        }
    }

    if !rule.enabled {
        info!("[Feishu-Comment] Comments disabled for {file_type}:{file_token}, skipping");
        return;
    }
    if !crate::platforms::feishu_comment_rules::is_user_allowed(&rule, from_open_id) {
        info!(
            "[Feishu-Comment] User {from_open_id} denied (policy={} rule={})",
            rule.policy, rule.match_source
        );
        return;
    }

    info!(
        "[Feishu-Comment] Access granted: user={from_open_id} policy={} rule={}",
        rule.policy, rule.match_source
    );

    if !reply_id.is_empty() {
        tokio::spawn({
            let client = client.clone();
            let token = token.to_string();
            let file_token = file_token.to_string();
            let file_type = file_type.to_string();
            let reply_id = reply_id.to_string();
            async move {
                add_comment_reaction(&client, &token, &file_token, &file_type, &reply_id, "OK").await;
            }
        });
    }

    // Step 2: Parallel fetch -- doc meta + comment details
    info!("[Feishu-Comment] [Step 2/5] Parallel fetch: doc meta + comment batch_query");
    let meta_fut = query_document_meta(client, token, file_token, file_type);
    let comment_fut = batch_query_comment(client, token, file_token, file_type, comment_id);
    let (doc_meta, comment_detail) = tokio::join!(meta_fut, comment_fut);

    let doc_title = doc_meta.get("title").map(|s| s.as_str()).unwrap_or("Untitled");
    let doc_url = doc_meta.get("url").map(|s| s.as_str()).unwrap_or("");
    let is_whole = comment_detail
        .get("is_whole")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    info!(
        "[Feishu-Comment] Comment context: title={doc_title} is_whole={is_whole}"
    );

    // Step 3: Build timeline based on comment type
    info!("[Feishu-Comment] [Step 3/5] Building timeline (is_whole={is_whole})");
    let prompt = if is_whole {
        build_whole_prompt(client, token, file_token, file_type, doc_title, doc_url, self_open_id, from_open_id).await
    } else {
        build_local_prompt(client, token, file_token, file_type, comment_id, doc_title, doc_url, self_open_id, from_open_id, reply_id, &comment_detail).await
    };

    info!(
        "[Feishu-Comment] [Step 4/5] Prompt built ({} chars), running agent...",
        prompt.len()
    );
    debug!("[Feishu-Comment] Full prompt:\n{prompt}");

    // Step 4: Run agent
    let _sess_key = _session_key(file_type, file_token);
    let response = _run_comment_agent(&prompt, message_handler).await;

    if response.is_empty() || response.contains(NO_REPLY_SENTINEL) {
        info!("[Feishu-Comment] Agent returned NO_REPLY, skipping delivery");
    } else {
        info!(
            "[Feishu-Comment] Agent response ({} chars): {}",
            response.len(),
            &response[..response.len().min(200)]
        );

        // Step 5: Deliver reply
        info!(
            "[Feishu-Comment] [Step 5/5] Delivering reply (is_whole={is_whole}, comment_id={comment_id})"
        );
        let success = deliver_comment_reply(
            client, token, file_token, file_type, comment_id, &response, is_whole,
        )
        .await;
        if success {
            info!("[Feishu-Comment] Reply delivered successfully");
        } else {
            error!("[Feishu-Comment] Failed to deliver reply");
        }
    }

    // Cleanup: remove OK reaction (best-effort)
    if !reply_id.is_empty() {
        delete_comment_reaction(
            client,
            token,
            file_token,
            file_type,
            reply_id,
            "OK",
        )
        .await;
    }

    info!("[Feishu-Comment] ========== handle_drive_comment_event END ==========");
}

async fn build_whole_prompt(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    doc_title: &str,
    doc_url: &str,
    self_open_id: &str,
    from_open_id: &str,
) -> String {
    info!("[Feishu-Comment] Fetching whole-document comments for timeline...");
    let whole_comments = list_whole_comments(client, token, file_token, file_type).await;

    let mut timeline: Vec<(String, String, bool)> = Vec::new();
    let mut current_text = String::new();
    let mut current_index: usize = 0;
    let mut nearest_self_index: usize = 0;

    for wc in &whole_comments {
        let reply_list = wc.get("reply_list").cloned().unwrap_or(Value::Null);
        let reply_list = if let Value::String(s) = reply_list {
            serde_json::from_str::<Value>(&s).unwrap_or(Value::Null)
        } else {
            reply_list
        };
        let replies = reply_list
            .get("replies")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for r in &replies {
            let uid = _get_reply_user_id(r);
            let text = _extract_reply_text(r);
            let is_self = !self_open_id.is_empty() && uid == self_open_id;
            let idx = timeline.len();
            timeline.push((uid.clone(), text.clone(), is_self));
            if uid == from_open_id {
                current_text = _extract_semantic_text(r, self_open_id);
                current_index = idx;
            }
            if is_self {
                nearest_self_index = idx;
            }
        }
    }

    if current_text.is_empty() {
        for (i, (_uid, text, is_self)) in timeline.iter().enumerate().rev() {
            if !is_self {
                current_text = text.clone();
                current_index = i;
                break;
            }
        }
    }

    info!(
        "[Feishu-Comment] Whole timeline: {} entries, current_idx={current_index}, self_idx={nearest_self_index}, text={}",
        timeline.len(),
        &current_text[..current_text.len().min(80)]
    );

    let mut all_raw_replies = Vec::new();
    for wc in &whole_comments {
        let rl = wc.get("reply_list").cloned().unwrap_or(Value::Null);
        let rl = if let Value::String(s) = rl {
            serde_json::from_str::<Value>(&s).unwrap_or(Value::Null)
        } else {
            rl
        };
        if let Some(replies) = rl.get("replies").and_then(|v| v.as_array()) {
            all_raw_replies.extend(replies.iter().cloned());
        }
    }
    let mut doc_links = _extract_docs_links(&all_raw_replies);
    if !doc_links.is_empty() {
        _resolve_wiki_nodes(client, token, &mut doc_links).await;
    }
    let ref_docs_text = _format_referenced_docs(&doc_links, file_token);

    build_whole_comment_prompt(
        doc_title,
        doc_url,
        file_token,
        file_type,
        &current_text,
        &timeline,
        self_open_id,
        current_index,
        nearest_self_index,
        &ref_docs_text,
    )
}

async fn build_local_prompt(
    client: &Client,
    token: &str,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    doc_title: &str,
    doc_url: &str,
    self_open_id: &str,
    from_open_id: &str,
    expect_reply_id: &str,
    comment_detail: &Value,
) -> String {
    info!("[Feishu-Comment] Fetching comment thread replies...");
    let replies = list_comment_replies(
        client,
        token,
        file_token,
        file_type,
        comment_id,
        expect_reply_id,
    )
    .await;

    // Extract quoted content from the comment detail (the document text the user highlighted).
    let quote_text = _extract_reply_text(comment_detail);

    let mut timeline: Vec<(String, String, bool)> = Vec::new();
    let mut root_text = String::new();
    let mut target_text = String::new();
    let mut target_index: usize = 0;

    for (i, r) in replies.iter().enumerate() {
        let uid = _get_reply_user_id(r);
        let text = _extract_reply_text(r);
        let is_self = !self_open_id.is_empty() && uid == self_open_id;
        timeline.push((uid.clone(), text.clone(), is_self));
        if i == 0 {
            root_text = _extract_semantic_text(r, self_open_id);
        }
        let rid = r.get("reply_id").and_then(|v| v.as_str()).unwrap_or("");
        if !rid.is_empty() && rid == expect_reply_id {
            target_text = _extract_semantic_text(r, self_open_id);
            target_index = i;
        }
    }

    if target_text.is_empty() && !timeline.is_empty() {
        for (i, (uid, text, _)) in timeline.iter().enumerate().rev() {
            if uid == from_open_id {
                target_text = text.clone();
                target_index = i;
                break;
            }
        }
    }

    info!(
        "[Feishu-Comment] Local timeline: {} entries, target_idx={target_index}, quote={} root={} target={}",
        timeline.len(),
        &quote_text[..quote_text.len().min(60)],
        &root_text[..root_text.len().min(60)],
        &target_text[..target_text.len().min(60)]
    );

    let mut doc_links = _extract_docs_links(&replies);
    if !doc_links.is_empty() {
        _resolve_wiki_nodes(client, token, &mut doc_links).await;
    }
    let ref_docs_text = _format_referenced_docs(&doc_links, file_token);

    build_local_comment_prompt(
        doc_title,
        doc_url,
        file_token,
        file_type,
        comment_id,
        &quote_text,
        &root_text,
        &target_text,
        &timeline,
        self_open_id,
        target_index,
        &ref_docs_text,
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_comment_text() {
        assert_eq!(
            _sanitize_comment_text("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn test_chunk_text() {
        let text = "line1\nline2\nline3";
        let chunks = _chunk_text(text, 100);
        assert_eq!(chunks, vec!["line1\nline2\nline3"]);

        let long = "a".repeat(5000);
        let chunks = _chunk_text(&long, 4000);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].len() <= 4000);
    }

    #[test]
    fn test_truncate_with_suffix() {
        assert_eq!(truncate_text_with_suffix("short", 10, "..."), "short");
        assert_eq!(truncate_text_with_suffix("1234567890abcdef", 10, "..."), "1234567...");
    }

    #[test]
    fn test_extract_reply_text() {
        let reply = serde_json::json!({
            "content": {
                "elements": [
                    {"type": "text_run", "text_run": {"text": "hello "}},
                    {"type": "person", "person": {"user_id": "u123"}},
                ]
            }
        });
        assert_eq!(_extract_reply_text(&reply), "hello @u123");
    }

    #[test]
    fn test_select_local_timeline() {
        let timeline: Vec<_> = (0..30)
            .map(|i| (format!("u{i}"), format!("text{i}"), false))
            .collect();
        let selected = _select_local_timeline(&timeline, 15);
        assert!(selected.len() <= LOCAL_TIMELINE_LIMIT);
        assert!(selected.iter().any(|(id, _, _)| id == "u0"));
        assert!(selected.iter().any(|(id, _, _)| id == "u29"));
        assert!(selected.iter().any(|(id, _, _)| id == "u15"));
    }

    #[test]
    fn test_parse_drive_comment_event() {
        let data = serde_json::json!({
            "event": {
                "event_id": "e1",
                "comment_id": "c1",
                "reply_id": "r1",
                "is_mentioned": true,
                "timestamp": "1234567890",
                "notice_meta": {
                    "file_token": "ftok",
                    "file_type": "docx",
                    "notice_type": "add_comment",
                    "from_user_id": {"open_id": "from123"},
                    "to_user_id": {"open_id": "to123"},
                }
            }
        });
        let evt = parse_drive_comment_event(&data).unwrap();
        assert_eq!(evt.event_id, "e1");
        assert_eq!(evt.comment_id, "c1");
        assert_eq!(evt.file_token, "ftok");
        assert_eq!(evt.file_type, "docx");
        assert_eq!(evt.from_open_id, "from123");
        assert_eq!(evt.to_open_id, "to123");
        assert!(evt.is_mentioned);
    }

    #[test]
    fn test_session_cache() {
        let key = _session_key("docx", "test_token");
        let msg = {
            let mut m = HashMap::new();
            m.insert("role".to_string(), "user".to_string());
            m.insert("content".to_string(), "hello".to_string());
            m
        };
        _save_session_history(&key, &[msg.clone()]);
        let loaded = _load_session_history(&key);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].get("role").unwrap(), "user");
    }
}
