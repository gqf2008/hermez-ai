#![allow(dead_code)]
//! Feishu (Lark) document tools.
//!
//! Mirrors Python `tools/feishu_doc_tool.py` + `tools/feishu_drive_tool.py`.
//! 6 tools: feishu_doc_read, feishu_drive_list_comments,
//! feishu_drive_list_comment_replies, feishu_drive_reply_comment,
//! feishu_drive_add_comment.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

const FEISHU_API_BASE: &str = "https://open.feishu.cn";

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TenantTokenReq {
    app_id: String,
    app_secret: String,
}

#[derive(Deserialize)]
struct TenantTokenResp {
    code: i32,
    msg: Option<String>,
    tenant_access_token: Option<String>,
}

fn get_credentials() -> Option<(String, String)> {
    let app_id = std::env::var("FEISHU_APP_ID").ok()?;
    let app_secret = std::env::var("FEISHU_APP_SECRET").ok()?;
    Some((app_id, app_secret))
}

/// Check if Feishu integration is available.
pub fn check_feishu_available() -> bool {
    std::env::var("FEISHU_APP_ID").is_ok() && std::env::var("FEISHU_APP_SECRET").is_ok()
}

async fn fetch_tenant_token(app_id: &str, app_secret: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{FEISHU_API_BASE}/open-apis/auth/v3/tenant_access_token/internal"))
        .json(&TenantTokenReq {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
        })
        .send()
        .await
        .map_err(|e| format!("Token request failed: {e}"))?;

    let body: TenantTokenResp = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {e}"))?;

    if body.code != 0 {
        return Err(format!(
            "Feishu auth error: code={} msg={}",
            body.code,
            body.msg.unwrap_or_default()
        ));
    }

    body.tenant_access_token
        .ok_or_else(|| "No tenant_access_token in response".to_string())
}

fn auth_header(token: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "Authorization",
        format!("Bearer {token}").parse().unwrap(),
    );
    h
}

fn run_async<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "No async runtime available".to_string())?;
    handle.block_on(fut)
}

// ---------------------------------------------------------------------------
// feishu_doc_read
// ---------------------------------------------------------------------------

async fn doc_read(token: &str, doc_token: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "{FEISHU_API_BASE}/open-apis/docx/v1/documents/{doc_token}/raw_content"
        ))
        .headers(auth_header(token))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if !status.is_success() {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu API error {}: {msg}", status));
    }

    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu business error: code={code} msg={msg}"));
    }

    let content = body
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Ok(content)
}

pub fn handle_feishu_doc_read(args: Value) -> Result<String, hermes_core::HermesError> {
    let (app_id, app_secret) = match get_credentials() {
        Some(c) => c,
        None => return Ok(tool_error("FEISHU_APP_ID and FEISHU_APP_SECRET not set.")),
    };

    let doc_token = args
        .get("doc_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if doc_token.is_empty() {
        return Ok(tool_error("doc_token is required"));
    }

    match run_async(async {
        let token = fetch_tenant_token(&app_id, &app_secret).await?;
        doc_read(&token, doc_token).await
    }) {
        Ok(content) => Ok(serde_json::json!({ "success": true, "content": content }).to_string()),
        Err(e) => Ok(tool_error(format!("Failed to read document: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// feishu_drive_list_comments
// ---------------------------------------------------------------------------

async fn list_comments(
    token: &str,
    file_token: &str,
    file_type: &str,
    is_whole: bool,
    page_size: i64,
    page_token: Option<&str>,
) -> Result<Value, String> {
    let mut url = format!(
        "{FEISHU_API_BASE}/open-apis/drive/v1/files/{file_token}/comments?file_type={file_type}&user_id_type=open_id&page_size={page_size}"
    );
    if is_whole {
        url.push_str("&is_whole=true");
    }
    if let Some(pt) = page_token {
        url.push_str(&format!("&page_token={pt}"));
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .headers(auth_header(token))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if !status.is_success() {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu API error {}: {msg}", status));
    }

    Ok(body)
}

pub fn handle_feishu_drive_list_comments(
    args: Value,
) -> Result<String, hermes_core::HermesError> {
    let (app_id, app_secret) = match get_credentials() {
        Some(c) => c,
        None => return Ok(tool_error("FEISHU_APP_ID and FEISHU_APP_SECRET not set.")),
    };

    let file_token = args
        .get("file_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if file_token.is_empty() {
        return Ok(tool_error("file_token is required"));
    }

    let file_type = args.get("file_type").and_then(Value::as_str).unwrap_or("docx");
    let is_whole = args.get("is_whole").and_then(Value::as_bool).unwrap_or(false);
    let page_size = args.get("page_size").and_then(Value::as_i64).unwrap_or(100);
    let page_token = args.get("page_token").and_then(Value::as_str);

    match run_async(async {
        let token = fetch_tenant_token(&app_id, &app_secret).await?;
        list_comments(
            &token,
            file_token,
            file_type,
            is_whole,
            page_size,
            page_token,
        )
        .await
    }) {
        Ok(data) => Ok(serde_json::json!({ "success": true, "data": data }).to_string()),
        Err(e) => Ok(tool_error(format!("List comments failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// feishu_drive_list_comment_replies
// ---------------------------------------------------------------------------

async fn list_replies(
    token: &str,
    file_token: &str,
    comment_id: &str,
    file_type: &str,
    page_size: i64,
    page_token: Option<&str>,
) -> Result<Value, String> {
    let mut url = format!(
        "{FEISHU_API_BASE}/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies?file_type={file_type}&user_id_type=open_id&page_size={page_size}"
    );
    if let Some(pt) = page_token {
        url.push_str(&format!("&page_token={pt}"));
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .headers(auth_header(token))
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if !status.is_success() {
        let msg = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu API error {}: {msg}", status));
    }

    Ok(body)
}

pub fn handle_feishu_drive_list_comment_replies(
    args: Value,
) -> Result<String, hermes_core::HermesError> {
    let (app_id, app_secret) = match get_credentials() {
        Some(c) => c,
        None => return Ok(tool_error("FEISHU_APP_ID and FEISHU_APP_SECRET not set.")),
    };

    let file_token = args
        .get("file_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let comment_id = args
        .get("comment_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if file_token.is_empty() || comment_id.is_empty() {
        return Ok(tool_error("file_token and comment_id are required"));
    }

    let file_type = args.get("file_type").and_then(Value::as_str).unwrap_or("docx");
    let page_size = args.get("page_size").and_then(Value::as_i64).unwrap_or(100);
    let page_token = args.get("page_token").and_then(Value::as_str);

    match run_async(async {
        let token = fetch_tenant_token(&app_id, &app_secret).await?;
        list_replies(
            &token,
            file_token,
            comment_id,
            file_type,
            page_size,
            page_token,
        )
        .await
    }) {
        Ok(data) => Ok(serde_json::json!({ "success": true, "data": data }).to_string()),
        Err(e) => Ok(tool_error(format!("List replies failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// feishu_drive_reply_comment
// ---------------------------------------------------------------------------

async fn reply_comment(
    token: &str,
    file_token: &str,
    comment_id: &str,
    content: &str,
    file_type: &str,
) -> Result<Value, String> {
    let url = format!(
        "{FEISHU_API_BASE}/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies?file_type={file_type}"
    );

    let body = serde_json::json!({
        "content": {
            "elements": [
                {
                    "type": "text_run",
                    "text_run": { "text": content }
                }
            ]
        }
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .headers(auth_header(token))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let response_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if !status.is_success() {
        let msg = response_body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu API error {}: {msg}", status));
    }

    Ok(response_body)
}

pub fn handle_feishu_drive_reply_comment(
    args: Value,
) -> Result<String, hermes_core::HermesError> {
    let (app_id, app_secret) = match get_credentials() {
        Some(c) => c,
        None => return Ok(tool_error("FEISHU_APP_ID and FEISHU_APP_SECRET not set.")),
    };

    let file_token = args
        .get("file_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let comment_id = args
        .get("comment_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if file_token.is_empty() || comment_id.is_empty() || content.is_empty() {
        return Ok(tool_error("file_token, comment_id, and content are required"));
    }

    let file_type = args.get("file_type").and_then(Value::as_str).unwrap_or("docx");

    match run_async(async {
        let token = fetch_tenant_token(&app_id, &app_secret).await?;
        reply_comment(&token, file_token, comment_id, content, file_type).await
    }) {
        Ok(data) => Ok(serde_json::json!({ "success": true, "data": data }).to_string()),
        Err(e) => Ok(tool_error(format!("Reply comment failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// feishu_drive_add_comment
// ---------------------------------------------------------------------------

async fn add_comment(
    token: &str,
    file_token: &str,
    content: &str,
    file_type: &str,
) -> Result<Value, String> {
    let url = format!(
        "{FEISHU_API_BASE}/open-apis/drive/v1/files/{file_token}/new_comments"
    );

    let body = serde_json::json!({
        "file_type": file_type,
        "reply_elements": [
            { "type": "text", "text": content }
        ]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .headers(auth_header(token))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let response_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if !status.is_success() {
        let msg = response_body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Feishu API error {}: {msg}", status));
    }

    Ok(response_body)
}

pub fn handle_feishu_drive_add_comment(
    args: Value,
) -> Result<String, hermes_core::HermesError> {
    let (app_id, app_secret) = match get_credentials() {
        Some(c) => c,
        None => return Ok(tool_error("FEISHU_APP_ID and FEISHU_APP_SECRET not set.")),
    };

    let file_token = args
        .get("file_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if file_token.is_empty() || content.is_empty() {
        return Ok(tool_error("file_token and content are required"));
    }

    let file_type = args.get("file_type").and_then(Value::as_str).unwrap_or("docx");

    match run_async(async {
        let token = fetch_tenant_token(&app_id, &app_secret).await?;
        add_comment(&token, file_token, content, file_type).await
    }) {
        Ok(data) => Ok(serde_json::json!({ "success": true, "data": data }).to_string()),
        Err(e) => Ok(tool_error(format!("Add comment failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_feishu_tools(registry: &mut ToolRegistry) {
    let check: Option<std::sync::Arc<crate::registry::CheckFn>> =
        Some(std::sync::Arc::new(check_feishu_available));
    let requires = vec!["FEISHU_APP_ID".to_string(), "FEISHU_APP_SECRET".to_string()];

    registry.register(
        "feishu_doc_read".to_string(),
        "feishu_doc".to_string(),
        serde_json::json!({
            "name": "feishu_doc_read",
            "description": "Read the full content of a Feishu/Lark document as plain text.",
            "parameters": {
                "type": "object",
                "properties": {
                    "doc_token": {
                        "type": "string",
                        "description": "The document token (from the document URL or comment context)."
                    }
                },
                "required": ["doc_token"]
            }
        }),
        std::sync::Arc::new(handle_feishu_doc_read),
        check.clone(),
        requires.clone(),
        "Read Feishu document content".to_string(),
        "📄".to_string(),
        None,
    );

    registry.register(
        "feishu_drive_list_comments".to_string(),
        "feishu_drive".to_string(),
        serde_json::json!({
            "name": "feishu_drive_list_comments",
            "description": "List comments on a Feishu document.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_token": { "type": "string", "description": "The document file token." },
                    "file_type": { "type": "string", "description": "File type (default: docx).", "default": "docx" },
                    "is_whole": { "type": "boolean", "description": "If true, only return whole-document comments.", "default": false },
                    "page_size": { "type": "integer", "description": "Number of comments per page (max 100).", "default": 100 },
                    "page_token": { "type": "string", "description": "Pagination token for next page." }
                },
                "required": ["file_token"]
            }
        }),
        std::sync::Arc::new(handle_feishu_drive_list_comments),
        check.clone(),
        requires.clone(),
        "List document comments".to_string(),
        "💬".to_string(),
        None,
    );

    registry.register(
        "feishu_drive_list_comment_replies".to_string(),
        "feishu_drive".to_string(),
        serde_json::json!({
            "name": "feishu_drive_list_comment_replies",
            "description": "List all replies in a comment thread on a Feishu document.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_token": { "type": "string", "description": "The document file token." },
                    "comment_id": { "type": "string", "description": "The comment ID to list replies for." },
                    "file_type": { "type": "string", "description": "File type (default: docx).", "default": "docx" },
                    "page_size": { "type": "integer", "description": "Number of replies per page (max 100).", "default": 100 },
                    "page_token": { "type": "string", "description": "Pagination token for next page." }
                },
                "required": ["file_token", "comment_id"]
            }
        }),
        std::sync::Arc::new(handle_feishu_drive_list_comment_replies),
        check.clone(),
        requires.clone(),
        "List comment replies".to_string(),
        "💬".to_string(),
        None,
    );

    registry.register(
        "feishu_drive_reply_comment".to_string(),
        "feishu_drive".to_string(),
        serde_json::json!({
            "name": "feishu_drive_reply_comment",
            "description": "Reply to a local comment thread on a Feishu document.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_token": { "type": "string", "description": "The document file token." },
                    "comment_id": { "type": "string", "description": "The comment ID to reply to." },
                    "content": { "type": "string", "description": "The reply text content (plain text only, no markdown)." },
                    "file_type": { "type": "string", "description": "File type (default: docx).", "default": "docx" }
                },
                "required": ["file_token", "comment_id", "content"]
            }
        }),
        std::sync::Arc::new(handle_feishu_drive_reply_comment),
        check.clone(),
        requires.clone(),
        "Reply to a document comment".to_string(),
        "✉️".to_string(),
        None,
    );

    registry.register(
        "feishu_drive_add_comment".to_string(),
        "feishu_drive".to_string(),
        serde_json::json!({
            "name": "feishu_drive_add_comment",
            "description": "Add a new whole-document comment on a Feishu document.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_token": { "type": "string", "description": "The document file token." },
                    "content": { "type": "string", "description": "The comment text content (plain text only, no markdown)." },
                    "file_type": { "type": "string", "description": "File type (default: docx).", "default": "docx" }
                },
                "required": ["file_token", "content"]
            }
        }),
        std::sync::Arc::new(handle_feishu_drive_add_comment),
        check,
        requires,
        "Add a whole-document comment".to_string(),
        "✉️".to_string(),
        None,
    );
}
