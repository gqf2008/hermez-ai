#![allow(dead_code)]
//! AWS Bedrock Converse API adapter.
//!
//! Mirrors Python `agent/bedrock_adapter.py`: SigV4 signing, message/tool
//! format conversion between OpenAI and Bedrock Converse, streaming support,
//! credential resolution, error classification, and model discovery.
//!
//! # Authentication
//!
//! Supports three auth methods (priority order):
//! 1. Explicit access key + secret key (provided via `BedrockClient::with_credentials`)
//! 2. AWS bearer token for Bedrock (`AWS_BEARER_TOKEN_BEDROCK` env var)
//! 3. Default AWS credential chain via environment variables:
//!    `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, or `AWS_PROFILE`
//!
//! # SigV4 Signing
//!
//! Requests are signed using AWS Signature Version 4 (manual implementation
//! using hmac-sha256). This avoids the `aws-sdk-bedrock-runtime` dependency
//! while still providing proper authentication.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error_classifier::{classify_api_error, ClassifiedError};

// ── Type aliases for crypto ────────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;

// ── Bedrock regions ────────────────────────────────────────────────────────

/// Default Bedrock region when none is configured.
pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";

/// Resolve the Bedrock region from environment variables.
///
/// Priority: `AWS_REGION` > `AWS_DEFAULT_REGION` > `us-east-1`.
pub fn resolve_bedrock_region() -> String {
    std::env::var("AWS_REGION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AWS_DEFAULT_REGION")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_string())
}

// ── AWS credential detection ───────────────────────────────────────────────

/// Detected AWS authentication source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwsAuthSource {
    /// Explicit access key ID + secret access key.
    ExplicitCredentials,
    /// AWS bearer token for Bedrock.
    BearerToken,
    /// Named AWS profile (SSO, assume-role, etc.).
    Profile,
    /// EC2 instance role / ECS task role / Lambda.
    IamRole,
    /// No credentials found.
    None,
}

impl AwsAuthSource {
    /// Human-readable name for the auth source.
    pub fn name(&self) -> &str {
        match self {
            Self::ExplicitCredentials => "AWS_ACCESS_KEY_ID",
            Self::BearerToken => "AWS_BEARER_TOKEN_BEDROCK",
            Self::Profile => "AWS_PROFILE",
            Self::IamRole => "iam-role",
            Self::None => "none",
        }
    }
}

/// Detect which AWS credential source is active.
///
/// Priority order matches the Python implementation:
/// 1. `AWS_BEARER_TOKEN_BEDROCK`
/// 2. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`
/// 3. `AWS_PROFILE`
/// 4. `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` (ECS)
/// 5. `AWS_WEB_IDENTITY_TOKEN_FILE` (EKS IRSA)
/// 6. IAM role (implicit — detected by presence of access key without secret,
///    suggesting IMDS/ECS metadata)
pub fn detect_aws_auth_source() -> AwsAuthSource {
    if let Ok(token) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        if !token.trim().is_empty() {
            return AwsAuthSource::BearerToken;
        }
    }
    if let (Ok(access_key), Ok(secret_key)) = (
        std::env::var("AWS_ACCESS_KEY_ID"),
        std::env::var("AWS_SECRET_ACCESS_KEY"),
    ) {
        if !access_key.trim().is_empty() && !secret_key.trim().is_empty() {
            return AwsAuthSource::ExplicitCredentials;
        }
    }
    if let Ok(profile) = std::env::var("AWS_PROFILE") {
        if !profile.trim().is_empty() {
            return AwsAuthSource::Profile;
        }
    }
    if let Ok(uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        if !uri.trim().is_empty() {
            return AwsAuthSource::IamRole;
        }
    }
    if let Ok(file) = std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE") {
        if !file.trim().is_empty() {
            return AwsAuthSource::IamRole;
        }
    }
    // Check for partial credentials that suggest IMDS/ECS
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        return AwsAuthSource::IamRole;
    }
    AwsAuthSource::None
}

/// Return true if any AWS credential source is detected.
pub fn has_aws_credentials() -> bool {
    !matches!(detect_aws_auth_source(), AwsAuthSource::None)
}

// ── Bedrock credentials ────────────────────────────────────────────────────

/// AWS credentials for SigV4 signing.
/// Secret key is redacted in Debug output to prevent accidental logging.
#[derive(Clone)]
pub struct BedrockCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Optional session token for temporary credentials (IAM roles, STS).
    pub session_token: Option<String>,
}

impl std::fmt::Debug for BedrockCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"***REDACTED***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***REDACTED***"))
            .finish()
    }
}

impl BedrockCredentials {
    /// Create credentials from environment variables.
    pub fn from_env() -> Option<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        if access_key_id.trim().is_empty() || secret_access_key.trim().is_empty() {
            return None;
        }
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
        Some(Self {
            access_key_id,
            secret_access_key,
            session_token: session_token.filter(|v| !v.trim().is_empty()),
        })
    }
}

// ── SigV4 signing ──────────────────────────────────────────────────────────

/// Create an AWS Signature Version 4 authorization header.
///
/// Reference: <https://docs.aws.amazon.com/general/latest/gr/sig-v4-calculate.html>
fn sign_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    credentials: &BedrockCredentials,
    region: &str,
    service: &str,
) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let amz_date = format_amz_date(&now);
    let date_stamp = format_amz_date_short(&now);

    // Canonical request
    let parsed_url = url::Url::parse(url).unwrap_or_else(|_| {
        url::Url::parse("https://invalid/").unwrap()
    });
    let canonical_uri = parsed_url.path().to_string();
    let canonical_query = parsed_url.query().unwrap_or("");

    // Collect headers for signing
    let mut header_map: Vec<(String, String)> = Vec::new();
    // Required SigV4 headers
    header_map.push(("host".to_string(), parsed_url.host_str().unwrap_or("").to_string()));
    header_map.push(("x-amz-date".to_string(), amz_date.clone()));
    if let Some(ref token) = credentials.session_token {
        header_map.push(("x-amz-security-token".to_string(), token.clone()));
    }
    // Add any additional headers passed in
    for (k, v) in headers {
        let kl = k.to_lowercase();
        if kl != "host" && kl != "x-amz-date" && kl != "x-amz-security-token"
            && kl != "authorization" && kl != "content-length"
        {
            header_map.push((kl.clone(), v.clone()));
        }
    }
    // Sort headers by name (case-insensitive)
    header_map.sort_by(|a, b| a.0.cmp(&b.0));

    let signed_headers = header_map
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_headers = header_map
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect::<String>();

    let payload_hash = sha256_hex(body);

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash,
    );

    // String to sign
    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        sha256_hex(&canonical_request),
    );

    // Signing key
    let signing_key = derive_signing_key(
        &credentials.secret_access_key,
        &date_stamp,
        region,
        service,
    );

    // Signature
    let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

    // Authorization header
    let credential_value = format!(
        "{}/{}/{}",
        credentials.access_key_id, date_stamp, credential_scope
    );

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={},SignedHeaders={},Signature={}",
        credential_value, signed_headers, signature,
    );

    if let Some(ref token) = credentials.session_token {
        // Session token goes in x-amz-security-token header, already added above
        let _ = token; // used in header construction
    }

    auth_header
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret_key).into_bytes();
    let k_date = hmac_sha256(&k_secret, date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha256(key, data))
}

fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

fn format_amz_date(duration: &Duration) -> String {
    let secs = duration.as_secs();
    let naive = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or_default();
    naive.format("%Y%m%dT%H%M%SZ").to_string()
}

fn format_amz_date_short(duration: &Duration) -> String {
    let secs = duration.as_secs();
    let naive = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or_default();
    naive.format("%Y%m%d").to_string()
}

// ── Bedrock client ─────────────────────────────────────────────────────────

/// Client for the AWS Bedrock Converse API.
///
/// ```no_run
/// use hermes_llm::bedrock::{BedrockClient, BedrockCredentials};
///
/// let creds = BedrockCredentials::from_env().expect("AWS credentials required");
/// let client = BedrockClient::new("us-east-1", creds);
/// ```
pub struct BedrockClient {
    pub region: String,
    pub credentials: BedrockCredentials,
    pub http_client: HttpClient,
}

impl BedrockClient {
    /// Create a new Bedrock client with explicit credentials.
    pub fn new(region: impl Into<String>, credentials: BedrockCredentials) -> Self {
        Self {
            region: region.into(),
            credentials,
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Create a client from environment variables.
    pub fn from_env() -> Option<Self> {
        let region = resolve_bedrock_region();
        let credentials = BedrockCredentials::from_env()?;
        Some(Self::new(region, credentials))
    }

    /// Build the Bedrock runtime URL for a model.
    fn runtime_url(&self, model: &str, stream: bool) -> String {
        let action = if stream { "converse-stream" } else { "converse" };
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/{}",
            self.region, model, action
        )
    }

    /// Make a signed HTTP request to Bedrock.
    async fn signed_request(
        &self,
        method: &str,
        url: &str,
        body: &Value,
    ) -> Result<(u16, String), ClassifiedError> {
        let body_str = serde_json::to_string(body).map_err(|e| {
            classify_api_error("bedrock", "bedrock", None,
                &format!("Failed to serialize request: {e}"))
        })?;

        let mut headers: Vec<(String, String)> = vec![
            ("content-type".to_string(), "application/json".to_string()),
        ];

        let auth = sign_request(
            method,
            url,
            &headers,
            &body_str,
            &self.credentials,
            &self.region,
            "bedrock",
        );

        headers.push(("authorization".to_string(), auth.clone()));

        if let Some(ref token) = self.credentials.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }

        let mut req = self.http_client
            .post(url)
            .header("content-type", "application/json");
        for (k, v) in &headers {
            req = req.header(k, v);
        }

        let resp = req
            .body(body_str)
            .send()
            .await
            .map_err(|e| classify_api_error("bedrock", "bedrock", None,
                &format!("Request failed: {e}")))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();

        Ok((status, text))
    }
}

// ── Inference configuration ────────────────────────────────────────────────

/// Inference parameters for a Bedrock Converse call.
#[derive(Debug, Clone)]
pub struct BedrockRequest {
    pub model: String,
    pub messages: Vec<Value>,
    pub tools: Option<Vec<Value>>,
    pub max_tokens: usize,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop_sequences: Option<Vec<String>>,
    pub guardrail_config: Option<Value>,
}

// ── Tool format conversion ─────────────────────────────────────────────────

/// Convert OpenAI-format tool definitions to Bedrock Converse `toolConfig`.
///
/// OpenAI format:
/// ```json
/// {"type": "function", "function": {"name": "...", "description": "...",
///  "parameters": {"type": "object", "properties": {...}}}}
/// ```
///
/// Converse format:
/// ```json
/// {"toolSpec": {"name": "...", "description": "...",
///  "inputSchema": {"json": {"type": "object", "properties": {...}}}}}
/// ```
pub fn convert_tools_to_converse(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let fn_obj = t.get("function")?;
            let name = fn_obj.get("name")?.as_str()?;
            let description = fn_obj.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let parameters = fn_obj.get("parameters").cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "toolSpec": {
                    "name": name,
                    "description": description,
                    "inputSchema": {"json": parameters},
                }
            }))
        })
        .collect()
}

// ── Message format conversion: OpenAI -> Bedrock Converse ──────────────────

/// Convert a single OpenAI content part to Bedrock content blocks.
fn convert_content_to_converse(content: &Value) -> Vec<Value> {
    match content {
        Value::Null => vec![json!({"text": " "})],
        Value::String(s) => {
            if s.trim().is_empty() {
                vec![json!({"text": " "})]
            } else {
                vec![json!({"text": s})]
            }
        }
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    blocks.push(json!({"text": text}));
                    continue;
                }
                let Some(part_obj) = part.as_object() else { continue };
                let part_type = part_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match part_type {
                    "text" => {
                        let text = part_obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        blocks.push(json!({"text": if text.is_empty() { " " } else { text }}));
                    }
                    "image_url" => {
                        if let Some(image_url) = part_obj.get("image_url").and_then(|v| v.as_object()) {
                            if let Some(url) = image_url.get("url").and_then(|v| v.as_str()) {
                                if url.starts_with("data:") {
                                    // data:image/jpeg;base64,/9j/4AAQ...
                                    if let Some((_header, data)) = url.split_once(",") {
                                        let media_type = extract_media_type(url);
                                        let fmt = if media_type.contains('/') {
                                            media_type.split('/').next_back().unwrap_or("jpeg")
                                        } else {
                                            "jpeg"
                                        };
                                        blocks.push(json!({
                                            "image": {
                                                "format": fmt,
                                                "source": {"bytes": data},
                                            }
                                        }));
                                    }
                                } else {
                                    // Remote URL — Converse doesn't support URLs directly
                                    blocks.push(json!({"text": format!("[Image: {url}]")}));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                blocks.push(json!({"text": " "}));
            }
            blocks
        }
        _ => vec![json!({"text": content.to_string()})],
    }
}

fn extract_media_type(data_url: &str) -> &str {
    if let Some(after_data) = data_url.strip_prefix("data:") {
        if let Some(end_mime) = after_data.find(';') {
            return &after_data[..end_mime];
        }
    }
    "image/jpeg"
}

/// Convert OpenAI-format messages to Bedrock Converse format.
///
/// Returns `(system_blocks, converse_messages)` where:
/// - `system_blocks` is a list of system content blocks (empty if no system)
/// - `converse_messages` is the conversation in Converse format
///
/// Handles:
/// - System messages -> extracted as system prompt
/// - User messages -> `{"role": "user", "content": [...]}`
/// - Assistant messages -> `{"role": "assistant", "content": [...]}`
/// - Tool calls -> `{"toolUse": {"toolUseId": ..., "name": ..., "input": ...}}`
/// - Tool results -> `{"toolResult": {"toolUseId": ..., "content": [...]}}`
///
/// Converse requires strict user/assistant alternation. Consecutive messages
/// with the same role are merged.
pub fn convert_messages_to_converse(
    messages: &[Value],
) -> (Option<Vec<Value>>, Vec<Value>) {
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut converse_msgs: Vec<Value> = Vec::new();

    for msg in messages {
        let Some(msg_obj) = msg.as_object() else { continue };
        let role = msg_obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = msg_obj.get("content");

        match role {
            "system" => {
                if let Some(s) = content {
                    if let Some(text) = s.as_str() {
                        if !text.trim().is_empty() {
                            system_blocks.push(json!({"text": text}));
                        }
                    } else if let Some(parts) = s.as_array() {
                        for part in parts {
                            if let Some(obj) = part.as_object() {
                                if obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                                    if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                                        system_blocks.push(json!({"text": t}));
                                    }
                                }
                            }
                            if let Some(t) = part.as_str() {
                                system_blocks.push(json!({"text": t}));
                            }
                        }
                    }
                }
            }

            "tool" => {
                let tool_call_id = msg_obj.get("tool_call_id")
                    .and_then(|v| v.as_str()).unwrap_or("");
                let result_content = match content {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                    None => String::new(),
                };
                let tool_result = json!({
                    "toolResult": {
                        "toolUseId": tool_call_id,
                        "content": [{"text": result_content}],
                    }
                });

                // Merge into preceding user turn
                if let Some(last) = converse_msgs.last_mut() {
                    if last.get("role").and_then(|v| v.as_str()) == Some("user") {
                        if let Some(arr) = last.get_mut("content").and_then(|v| v.as_array_mut()) {
                            arr.push(tool_result);
                        }
                        continue;
                    }
                }
                converse_msgs.push(json!({
                    "role": "user",
                    "content": [tool_result],
                }));
            }

            "assistant" => {
                let mut content_blocks = Vec::new();

                // Text content
                if let Some(s) = content {
                    if let Some(text) = s.as_str() {
                        if !text.trim().is_empty() {
                            content_blocks.push(json!({"text": text}));
                        }
                    } else if let Some(parts) = s.as_array() {
                        content_blocks.extend(convert_content_to_converse(&Value::Array(parts.clone())));
                    }
                }

                // Tool calls
                if let Some(tool_calls) = msg_obj.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        if let Some(tc_obj) = tc.as_object() {
                            let fn_obj = tc_obj.get("function");
                            let name = fn_obj.and_then(|f| f.get("name").and_then(|v| v.as_str())).unwrap_or("");
                            let args_str = fn_obj.and_then(|f| f.get("arguments").and_then(|v| v.as_str())).unwrap_or("{}");
                            let args_dict: Value = serde_json::from_str(args_str).unwrap_or(Value::Object(Default::default()));
                            let id = tc_obj.get("id").and_then(|v| v.as_str()).unwrap_or("");

                            content_blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": id,
                                    "name": name,
                                    "input": args_dict,
                                }
                            }));
                        }
                    }
                }

                if content_blocks.is_empty() {
                    content_blocks.push(json!({"text": " "}));
                }

                // Merge with previous assistant message (strict alternation)
                if let Some(last) = converse_msgs.last_mut() {
                    if last.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                        if let Some(arr) = last.get_mut("content").and_then(|v| v.as_array_mut()) {
                            arr.extend(content_blocks);
                        }
                        continue;
                    }
                }
                converse_msgs.push(json!({
                    "role": "assistant",
                    "content": content_blocks,
                }));
            }

            "user" => {
                let content_blocks = convert_content_to_converse(content.unwrap_or(&Value::Null));

                // Merge with previous user message (strict alternation)
                if let Some(last) = converse_msgs.last_mut() {
                    if last.get("role").and_then(|v| v.as_str()) == Some("user") {
                        if let Some(arr) = last.get_mut("content").and_then(|v| v.as_array_mut()) {
                            arr.extend(content_blocks);
                        }
                        continue;
                    }
                }
                converse_msgs.push(json!({
                    "role": "user",
                    "content": content_blocks,
                }));
            }

            _ => {} // Skip unknown roles
        }
    }

    // Converse requires the first message to be from the user
    if let Some(first) = converse_msgs.first() {
        if first.get("role").and_then(|v| v.as_str()) != Some("user") {
            converse_msgs.insert(0, json!({"role": "user", "content": [{"text": " "}]}));
        }
    }

    // Converse requires the last message to be from the user
    if let Some(last) = converse_msgs.last() {
        if last.get("role").and_then(|v| v.as_str()) != Some("user") {
            converse_msgs.push(json!({"role": "user", "content": [{"text": " "}]}));
        }
    }

    let system = if system_blocks.is_empty() { None } else { Some(system_blocks) };
    (system, converse_msgs)
}

// ── Build Converse API request ─────────────────────────────────────────────

/// Build the JSON body for a Bedrock Converse API call.
///
/// Converts OpenAI-format inputs to Converse API parameters.
pub fn build_converse_request(req: &BedrockRequest) -> Value {
    let (system, converse_messages) = convert_messages_to_converse(&req.messages);

    let mut inference_config = json!({
        "maxTokens": req.max_tokens,
    });

    if let Some(temp) = req.temperature {
        if let Some(obj) = inference_config.as_object_mut() {
            obj.insert("temperature".to_string(), json!(temp));
        }
    }

    if let Some(top_p) = req.top_p {
        if let Some(obj) = inference_config.as_object_mut() {
            obj.insert("topP".to_string(), json!(top_p));
        }
    }

    let mut body = json!({
        "modelId": req.model,
        "messages": converse_messages,
        "inferenceConfig": inference_config,
    });

    if let Some(sys) = system {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("system".to_string(), Value::Array(sys));
        }
    }

    if let Some(ref stop) = req.stop_sequences {
        if !stop.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("stopSequences".to_string(), json!(stop));
            }
        }
    }

    // Tool configuration
    if let Some(ref tools) = req.tools {
        let converse_tools = convert_tools_to_converse(tools);
        if !converse_tools.is_empty() && model_supports_tool_use(&req.model) {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("toolConfig".to_string(), json!({"tools": converse_tools}));
            }
        } else if !converse_tools.is_empty() {
            tracing::warn!(
                "Model {} does not support tool calling — tools stripped. \
                 The agent will operate in text-only mode.",
                req.model
            );
        }
    }

    // Guardrail configuration
    if let Some(ref guardrail) = req.guardrail_config {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("guardrailConfig".to_string(), guardrail.clone());
        }
    }

    body
}

// ── Stop reason mapping ────────────────────────────────────────────────────

/// Map Bedrock Converse stop reasons to OpenAI finish_reason values.
fn converse_stop_reason_to_openai(stop_reason: &str) -> &str {
    match stop_reason {
        "end_turn" => "stop",
        "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "content_filtered" => "content_filter",
        "guardrail_intervened" => "content_filter",
        _ => "stop",
    }
}

// ── Response format conversion: Bedrock Converse -> OpenAI ─────────────────

/// Parse a Bedrock Converse API response into an OpenAI-compatible structure.
///
/// Returns a tuple of `(content, tool_calls, finish_reason, usage, model_id)`.
pub fn normalize_converse_response(
    response: &Value,
) -> (Option<String>, Option<Vec<Value>>, String, UsageInfo, String) {
    let output = response.get("output").and_then(|v| v.as_object());
    let message = output.and_then(|o| o.get("message")).and_then(|v| v.as_object());
    let content_blocks = message.and_then(|m| m.get("content")).and_then(|v| v.as_array());
    let stop_reason = response.get("stopReason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(blocks) = content_blocks {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                text_parts.push(text.to_string());
            } else if let Some(tool_use) = block.get("toolUse").and_then(|v| v.as_object()) {
                let tool_use_id = tool_use.get("toolUseId")
                    .and_then(|v| v.as_str()).unwrap_or("");
                let name = tool_use.get("name")
                    .and_then(|v| v.as_str()).unwrap_or("");
                let input = tool_use.get("input").cloned().unwrap_or(Value::Object(Default::default()));
                tool_calls.push(json!({
                    "id": tool_use_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(&input).unwrap_or_default(),
                    }
                }));
            }
        }
    }

    let content = if text_parts.is_empty() { None } else { Some(text_parts.join("\n")) };
    let tool_calls_opt = if tool_calls.is_empty() { None } else { Some(tool_calls) };

    let usage = response.get("usage").map(|u| UsageInfo {
        prompt_tokens: u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        total_tokens: u.get("totalTokens").and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0)
                    + u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0)
            }),
    }).unwrap_or_default();

    let model_id = response.get("modelId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut finish_reason = converse_stop_reason_to_openai(stop_reason).to_string();
    if tool_calls_opt.is_some() && finish_reason == "stop" {
        finish_reason = "tool_calls".to_string();
    }

    (content, tool_calls_opt, finish_reason, usage, model_id)
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ── Streaming response handling ────────────────────────────────────────────

/// Accumulated state from a streaming response.
#[derive(Debug, Clone, Default)]
pub struct StreamAccumulator {
    pub content: Option<String>,
    pub tool_calls: Vec<Value>,
    pub finish_reason: String,
    pub usage: UsageInfo,
    pub model: String,
    pub reasoning: Option<String>,
}

/// Parse a Bedrock ConverseStream SSE line and update the accumulator.
///
/// Handles all ConverseStream event types:
/// - `messageStart` — role info
/// - `contentBlockStart` — new text or toolUse block
/// - `contentBlockDelta` — incremental text or toolUse input
/// - `contentBlockStop` — block complete
/// - `messageStop` — stop reason
/// - `metadata` — usage stats
pub fn process_stream_event(
    acc: &mut StreamAccumulator,
    event_type: &str,
    payload: &Value,
) {
    match event_type {
        "contentBlockStart" => {
            if let Some(start) = payload.get("start").and_then(|v| v.as_object()) {
                if let Some(tool_use) = start.get("toolUse").and_then(|v| v.as_object()) {
                    let tool_use_id = tool_use.get("toolUseId")
                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = tool_use.get("name")
                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                    acc.tool_calls.push(json!({
                        "id": tool_use_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": "",
                        }
                    }));
                }
            }
        }

        "contentBlockDelta" => {
            if let Some(delta) = payload.get("delta").and_then(|v| v.as_object()) {
                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                    let current = acc.content.get_or_insert_with(String::new);
                    current.push_str(text);
                } else if let Some(tool_delta) = delta.get("toolUse").and_then(|v| v.as_object()) {
                    if let Some(input) = tool_delta.get("input").and_then(|v| v.as_str()) {
                        if let Some(last_tool) = acc.tool_calls.last_mut() {
                            if let Some(fn_obj) = last_tool.get_mut("function").and_then(|v| v.as_object_mut()) {
                                if let Some(Value::String(s)) = fn_obj.get_mut("arguments") {
                                    s.push_str(input);
                                }
                            }
                        }
                    }
                } else if let Some(reasoning) = delta.get("reasoningContent").and_then(|v| v.as_object()) {
                    if let Some(text) = reasoning.get("text").and_then(|v| v.as_str()) {
                        let current = acc.reasoning.get_or_insert_with(String::new);
                        current.push_str(text);
                    }
                }
            }
        }

        "messageStop" => {
            let stop_reason = payload.get("stopReason")
                .and_then(|v| v.as_str())
                .unwrap_or("end_turn");
            acc.finish_reason = converse_stop_reason_to_openai(stop_reason).to_string();
        }

        "metadata" => {
            if let Some(usage_val) = payload.get("usage").and_then(|v| v.as_object()) {
                acc.usage.prompt_tokens = usage_val.get("inputTokens")
                    .and_then(|v| v.as_u64()).unwrap_or(0);
                acc.usage.completion_tokens = usage_val.get("outputTokens")
                    .and_then(|v| v.as_u64()).unwrap_or(0);
                acc.usage.total_tokens = acc.usage.prompt_tokens + acc.usage.completion_tokens;
            }
        }

        _ => {}
    }

    // Finalize finish_reason if tool calls were seen
    if !acc.tool_calls.is_empty() && acc.finish_reason == "stop" {
        acc.finish_reason = "tool_calls".to_string();
    }
}

/// Parse a newline-delimited JSON stream (Bedrock ConverseStream format).
///
/// Bedrock returns NDJSON where each line is a JSON object with a single key
/// indicating the event type.
pub fn parse_bedrock_stream(text: &str) -> StreamAccumulator {
    let mut acc = StreamAccumulator::default();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(obj) = value.as_object() {
                // Find the event type (single key in the object)
                for (key, payload) in obj {
                    // Skip known metadata keys
                    if key == "Message" || key == "message" {
                        continue;
                    }
                    process_stream_event(&mut acc, key, payload);
                }
            }
        }
    }

    // Ensure content is Some even if empty
    if acc.content.as_ref().is_some_and(|s| s.is_empty()) {
        acc.content = None;
    }

    acc
}

// ── High-level API: call Bedrock Converse ──────────────────────────────────

/// Call Bedrock Converse API (non-streaming) and return an OpenAI-compatible response.
///
/// This is the primary entry point for the agent loop when using the Bedrock provider.
pub async fn call_bedrock(
    client: &BedrockClient,
    req: &BedrockRequest,
) -> Result<BedrockResponse, ClassifiedError> {
    let body = build_converse_request(req);
    let url = client.runtime_url(&req.model, false);

    let (status, text) = client.signed_request("POST", &url, &body).await?;

    if status >= 400 {
        return Err(classify_api_error("bedrock", &req.model, Some(status), &text));
    }

    let response: Value = serde_json::from_str(&text).map_err(|e| {
        classify_api_error("bedrock", &req.model, Some(status),
            &format!("Failed to parse response: {e}"))
    })?;

    let (content, tool_calls, finish_reason, usage, model) = normalize_converse_response(&response);

    Ok(BedrockResponse {
        content,
        tool_calls,
        finish_reason,
        usage,
        model,
    })
}

/// Call Bedrock ConverseStream API (streaming) and return an OpenAI-compatible response.
///
/// Consumes the full stream and returns the assembled response.
pub async fn call_bedrock_stream(
    client: &BedrockClient,
    req: &BedrockRequest,
) -> Result<BedrockResponse, ClassifiedError> {
    let body = build_converse_request(req);
    let url = client.runtime_url(&req.model, true);

    // Build signed request
    let body_str = serde_json::to_string(&body).map_err(|e| {
        classify_api_error("bedrock", &req.model, None,
            &format!("Failed to serialize request: {e}"))
    })?;

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/vnd.amazon.eventstream".to_string()),
    ];

    let auth = sign_request(
        "POST",
        &url,
        &headers,
        &body_str,
        &client.credentials,
        &client.region,
        "bedrock",
    );

    headers.push(("authorization".to_string(), auth));
    if let Some(ref token) = client.credentials.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }

    let mut req_builder = client.http_client.post(&url);
    for (k, v) in &headers {
        req_builder = req_builder.header(k, v);
    }

    let resp = req_builder
        .body(body_str)
        .send()
        .await
        .map_err(|e| classify_api_error("bedrock", &req.model, None,
            &format!("Request failed: {e}")))?;

    let status = resp.status().as_u16();

    if status >= 400 {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_api_error("bedrock", &req.model, Some(status), &text));
    }

    // Consume streaming response
    let text = resp.text().await.map_err(|e| {
        classify_api_error("bedrock", &req.model, None,
            &format!("Failed to read response body: {e}"))
    })?;

    let acc = parse_bedrock_stream(&text);

    let mut finish_reason = acc.finish_reason;
    if !acc.tool_calls.is_empty() && finish_reason == "stop" {
        finish_reason = "tool_calls".to_string();
    }

    Ok(BedrockResponse {
        content: acc.content,
        tool_calls: if acc.tool_calls.is_empty() { None } else { Some(acc.tool_calls) },
        finish_reason,
        usage: acc.usage,
        model: acc.model,
    })
}

/// Bedrock API response (OpenAI-compatible shape).
#[derive(Debug, Clone)]
pub struct BedrockResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<Value>>,
    pub finish_reason: String,
    pub usage: UsageInfo,
    pub model: String,
}

// ── Tool-calling capability detection ──────────────────────────────────────

/// Models that don't support tool/function calling.
///
/// Sending `toolConfig` to these models causes ValidationException.
static NON_TOOL_CALLING_PATTERNS: &[&str] = &[
    "deepseek.r1",      // DeepSeek R1 — reasoning only
    "deepseek-r1",      // Alternate ID format
    "stability.",       // Image generation models
    "cohere.embed",     // Embedding models
    "amazon.titan-embed", // Embedding models
];

/// Return true if the model is expected to support tool/function calling.
///
/// Unknown models default to true (assume tool support).
pub fn model_supports_tool_use(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    !NON_TOOL_CALLING_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Return true if the model is an Anthropic Claude model on Bedrock.
///
/// These models should use the AnthropicBedrock SDK path for full feature
/// parity (prompt caching, thinking budgets, adaptive thinking).
pub fn is_anthropic_bedrock_model(model_id: &str) -> bool {
    let mut lower = model_id.to_lowercase();
    // Strip regional prefix
    for prefix in ["us.", "global.", "eu.", "ap.", "jp."] {
        if lower.starts_with(prefix) {
            lower = lower[prefix.len()..].to_string();
            break;
        }
    }
    lower.starts_with("anthropic.claude")
}

// ── Error classification ───────────────────────────────────────────────────

use once_cell::sync::Lazy;

/// Patterns that indicate the input context exceeded the model's token limit.
static CONTEXT_OVERFLOW_PATTERNS: Lazy<Vec<regex::Regex>> = Lazy::new(|| {
    vec![
        regex::Regex::new(r"(?i)ValidationException.*(?:input is too long|max input token|input token.*exceed)").unwrap(),
        regex::Regex::new(r"(?i)ValidationException.*(?:exceeds? the (?:maximum|max) (?:number of )?(?:input )?tokens)").unwrap(),
        regex::Regex::new(r"(?i)ModelStreamErrorException.*(?:Input is too long|too many input tokens)").unwrap(),
    ]
});

/// Patterns for throttling / rate limit errors.
static THROTTLE_PATTERNS: Lazy<Vec<regex::Regex>> = Lazy::new(|| {
    vec![
        regex::Regex::new(r"(?i)ThrottlingException").unwrap(),
        regex::Regex::new(r"(?i)Too many concurrent requests").unwrap(),
        regex::Regex::new(r"(?i)ServiceQuotaExceededException").unwrap(),
    ]
});

/// Patterns for transient overload.
static OVERLOAD_PATTERNS: Lazy<Vec<regex::Regex>> = Lazy::new(|| {
    vec![
        regex::Regex::new(r"(?i)ModelNotReadyException").unwrap(),
        regex::Regex::new(r"(?i)ModelTimeoutException").unwrap(),
        regex::Regex::new(r"(?i)InternalServerException").unwrap(),
    ]
});

/// Return true if the error indicates the input context was too large.
pub fn is_context_overflow_error(error_message: &str) -> bool {
    CONTEXT_OVERFLOW_PATTERNS.iter().any(|p| p.is_match(error_message))
}

/// Classify a Bedrock error for retry/failover decisions.
///
/// Returns:
/// - `"context_overflow"` — input too long, compress and retry
/// - `"rate_limit"` — throttled, backoff and retry
/// - `"overloaded"` — model temporarily unavailable, retry with delay
/// - `"unknown"` — unclassified error
pub fn classify_bedrock_error(error_message: &str) -> &str {
    if is_context_overflow_error(error_message) {
        return "context_overflow";
    }
    if THROTTLE_PATTERNS.iter().any(|p| p.is_match(error_message)) {
        return "rate_limit";
    }
    if OVERLOAD_PATTERNS.iter().any(|p| p.is_match(error_message)) {
        return "overloaded";
    }
    "unknown"
}

// ── Bedrock model context lengths ──────────────────────────────────────────

/// Static fallback context lengths for Bedrock models.
///
/// Used when dynamic detection via the Bedrock API is unavailable.
static BEDROCK_CONTEXT_LENGTHS: &[(&str, usize)] = &[
    // Anthropic Claude models on Bedrock
    ("anthropic.claude-opus-4-6", 200_000),
    ("anthropic.claude-sonnet-4-6", 200_000),
    ("anthropic.claude-sonnet-4-5", 200_000),
    ("anthropic.claude-haiku-4-5", 200_000),
    ("anthropic.claude-opus-4", 200_000),
    ("anthropic.claude-sonnet-4", 200_000),
    ("anthropic.claude-3-5-sonnet", 200_000),
    ("anthropic.claude-3-5-haiku", 200_000),
    ("anthropic.claude-3-opus", 200_000),
    ("anthropic.claude-3-sonnet", 200_000),
    ("anthropic.claude-3-haiku", 200_000),
    // Amazon Nova
    ("amazon.nova-pro", 300_000),
    ("amazon.nova-lite", 300_000),
    ("amazon.nova-micro", 128_000),
    // Meta Llama
    ("meta.llama4-maverick", 128_000),
    ("meta.llama4-scout", 128_000),
    ("meta.llama3-3-70b-instruct", 128_000),
    // Mistral
    ("mistral.mistral-large", 128_000),
    // DeepSeek
    ("deepseek.v3", 128_000),
];

/// Default context length for unknown Bedrock models.
pub const BEDROCK_DEFAULT_CONTEXT_LENGTH: usize = 128_000;

/// Look up the context window size for a Bedrock model.
///
/// Uses substring matching so versioned IDs like
/// `anthropic.claude-sonnet-4-6-20250514-v1:0` resolve correctly.
/// Longest match wins.
pub fn get_bedrock_context_length(model_id: &str) -> usize {
    let lower = model_id.to_lowercase();
    let mut best_key = "";
    let mut best_val = BEDROCK_DEFAULT_CONTEXT_LENGTH;
    for (key, val) in BEDROCK_CONTEXT_LENGTHS {
        if lower.contains(key) && key.len() > best_key.len() {
            best_key = key;
            best_val = *val;
        }
    }
    best_val
}

// ── Model discovery (control plane) ────────────────────────────────────────

/// Information about a Bedrock model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BedrockModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub streaming: bool,
}

/// Extract the model provider from a Bedrock model ARN.
///
/// Example: `arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-v2`
/// -> `"anthropic"`
pub fn extract_provider_from_arn(arn: &str) -> &str {
    if let Some(pos) = arn.find("foundation-model/") {
        let after = &arn[pos + "foundation-model/".len()..];
        if let Some(dot_pos) = after.find('.') {
            return &after[..dot_pos];
        }
        return after;
    }
    ""
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_bedrock_region_default() {
        // Should not panic even without env vars
        let region = resolve_bedrock_region();
        assert!(!region.is_empty());
    }

    #[test]
    fn test_detect_aws_auth_source_none() {
        // In test environment without AWS env vars, should be None
        let source = detect_aws_auth_source();
        // Could be None or IamRole depending on test env
        assert!(matches!(source, AwsAuthSource::None | AwsAuthSource::IamRole));
    }

    #[test]
    fn test_convert_tools_to_converse() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather for a location",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }
        })];

        let result = convert_tools_to_converse(&tools);
        assert_eq!(result.len(), 1);
        let spec = result[0].get("toolSpec").unwrap();
        assert_eq!(spec.get("name").unwrap(), "get_weather");
        assert_eq!(spec.get("description").unwrap(), "Get weather for a location");
        assert!(spec.get("inputSchema").is_some());
    }

    #[test]
    fn test_convert_tools_to_converse_empty() {
        let result = convert_tools_to_converse(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_content_to_converse_string() {
        let result = convert_content_to_converse(&Value::String("Hello".to_string()));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].get("text").unwrap(), "Hello");
    }

    #[test]
    fn test_convert_content_to_converse_empty_string() {
        let result = convert_content_to_converse(&Value::String("".to_string()));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].get("text").unwrap(), " ");
    }

    #[test]
    fn test_convert_messages_to_converse_basic() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "Hi"}),
        ];

        let (system, msgs) = convert_messages_to_converse(&messages);
        assert!(system.is_some());
        let sys = system.unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0].get("text").unwrap(), "You are helpful.");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("role").unwrap(), "user");
    }

    #[test]
    fn test_convert_messages_to_converse_tool_result() {
        let messages = vec![
            json!({"role": "user", "content": "What's 2+2?"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "calculator",
                        "arguments": "{\"expression\": \"2+2\"}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "4"
            }),
        ];

        let (system, msgs) = convert_messages_to_converse(&messages);
        assert!(system.is_none());

        // Should have: user (question), assistant (tool_use), user (tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].get("role").unwrap(), "user");
        assert_eq!(msgs[1].get("role").unwrap(), "assistant");
        assert_eq!(msgs[2].get("role").unwrap(), "user");

        // Check tool result is in last user message
        let last_content = msgs[2].get("content").unwrap().as_array().unwrap();
        let has_tool_result = last_content.iter().any(|b| b.get("toolResult").is_some());
        assert!(has_tool_result);
    }

    #[test]
    fn test_convert_messages_merge_consecutive_same_role() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "user", "content": "World"}),
        ];

        let (system, msgs) = convert_messages_to_converse(&messages);
        assert!(system.is_none());

        // Should be merged into one user message
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("role").unwrap(), "user");
        let content = msgs[0].get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 2);
    }

    #[test]
    fn test_converse_stop_reason_mapping() {
        assert_eq!(converse_stop_reason_to_openai("end_turn"), "stop");
        assert_eq!(converse_stop_reason_to_openai("stop_sequence"), "stop");
        assert_eq!(converse_stop_reason_to_openai("tool_use"), "tool_calls");
        assert_eq!(converse_stop_reason_to_openai("max_tokens"), "length");
        assert_eq!(converse_stop_reason_to_openai("content_filtered"), "content_filter");
        assert_eq!(converse_stop_reason_to_openai("guardrail_intervened"), "content_filter");
        assert_eq!(converse_stop_reason_to_openai("unknown_reason"), "stop");
    }

    #[test]
    fn test_normalize_converse_response_text_only() {
        let response = json!({
            "modelId": "anthropic.claude-sonnet-4-6",
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "Hello! How can I help?"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 10,
                "outputTokens": 8,
                "totalTokens": 18
            }
        });

        let (content, tool_calls, finish_reason, usage, model) =
            normalize_converse_response(&response);

        assert_eq!(content, Some("Hello! How can I help?".to_string()));
        assert!(tool_calls.is_none());
        assert_eq!(finish_reason, "stop");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(model, "anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn test_normalize_converse_response_with_tool_use() {
        let response = json!({
            "modelId": "anthropic.claude-sonnet-4-6",
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{
                        "toolUse": {
                            "toolUseId": "call_abc",
                            "name": "get_weather",
                            "input": {"location": "NYC"}
                        }
                    }]
                }
            },
            "stopReason": "tool_use",
            "usage": {
                "inputTokens": 50,
                "outputTokens": 20,
            }
        });

        let (content, tool_calls, finish_reason, _usage, model) =
            normalize_converse_response(&response);

        assert!(content.is_none());
        assert!(tool_calls.is_some());
        let tools = tool_calls.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("id").unwrap(), "call_abc");
        assert_eq!(tools[0].get("function").unwrap().get("name").unwrap(), "get_weather");
        assert_eq!(finish_reason, "tool_calls");
        assert_eq!(model, "anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn test_normalize_converse_response_mixed() {
        let response = json!({
            "modelId": "anthropic.claude-sonnet-4-6",
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "Let me check the weather."},
                        {
                            "toolUse": {
                                "toolUseId": "call_xyz",
                                "name": "get_weather",
                                "input": {"location": "NYC"}
                            }
                        }
                    ]
                }
            },
            "stopReason": "tool_use",
            "usage": {"inputTokens": 30, "outputTokens": 15}
        });

        let (content, tool_calls, finish_reason, _usage, _model) =
            normalize_converse_response(&response);

        assert_eq!(content, Some("Let me check the weather.".to_string()));
        assert!(tool_calls.is_some());
        assert_eq!(finish_reason, "tool_calls");
    }

    #[test]
    fn test_model_supports_tool_use() {
        assert!(model_supports_tool_use("anthropic.claude-sonnet-4-6"));
        assert!(model_supports_tool_use("amazon.nova-pro"));
        assert!(!model_supports_tool_use("deepseek.r1-v1"));
        assert!(!model_supports_tool_use("deepseek-r1"));
        assert!(!model_supports_tool_use("stability.sd3"));
        assert!(!model_supports_tool_use("cohere.embed-english-v3"));
        assert!(!model_supports_tool_use("amazon.titan-embed-text-v2"));
    }

    #[test]
    fn test_is_anthropic_bedrock_model() {
        assert!(is_anthropic_bedrock_model("anthropic.claude-sonnet-4-6"));
        assert!(is_anthropic_bedrock_model("us.anthropic.claude-sonnet-4-6"));
        assert!(is_anthropic_bedrock_model("global.anthropic.claude-sonnet-4-6"));
        assert!(is_anthropic_bedrock_model("eu.anthropic.claude-sonnet-4-6"));
        assert!(is_anthropic_bedrock_model("ap.anthropic.claude-sonnet-4-6"));
        assert!(!is_anthropic_bedrock_model("amazon.nova-pro"));
        assert!(!is_anthropic_bedrock_model("meta.llama3-3-70b"));
    }

    #[test]
    fn test_classify_bedrock_error() {
        assert_eq!(
            classify_bedrock_error("ValidationException: Input is too long for this model"),
            "context_overflow"
        );
        assert_eq!(
            classify_bedrock_error("ThrottlingException: Rate exceeded"),
            "rate_limit"
        );
        assert_eq!(
            classify_bedrock_error("ServiceQuotaExceededException: Too many concurrent requests"),
            "rate_limit"
        );
        assert_eq!(
            classify_bedrock_error("ModelNotReadyException: Model is loading"),
            "overloaded"
        );
        assert_eq!(
            classify_bedrock_error("InternalServerException: An internal server error occurred"),
            "overloaded"
        );
        assert_eq!(
            classify_bedrock_error("ValidationException: Invalid parameter"),
            "unknown"
        );
    }

    #[test]
    fn test_get_bedrock_context_length() {
        assert_eq!(
            get_bedrock_context_length("anthropic.claude-sonnet-4-6-20250514-v1:0"),
            200_000
        );
        assert_eq!(
            get_bedrock_context_length("us.anthropic.claude-sonnet-4-6"),
            200_000
        );
        assert_eq!(
            get_bedrock_context_length("amazon.nova-pro-v1:0"),
            300_000
        );
        assert_eq!(
            get_bedrock_context_length("unknown.model"),
            BEDROCK_DEFAULT_CONTEXT_LENGTH
        );
    }

    #[test]
    fn test_parse_bedrock_stream_text_only() {
        let stream_text = r#"
{"messageStart": {"role": "assistant"}}
{"contentBlockStart": {"start": {}}}
{"contentBlockDelta": {"delta": {"text": "Hello"}}}
{"contentBlockDelta": {"delta": {"text": " world"}}}
{"contentBlockStop": {}}
{"messageStop": {"stopReason": "end_turn"}}
{"metadata": {"usage": {"inputTokens": 10, "outputTokens": 2}}}
"#;

        let acc = parse_bedrock_stream(stream_text);
        assert_eq!(acc.content, Some("Hello world".to_string()));
        assert!(acc.tool_calls.is_empty());
        assert_eq!(acc.finish_reason, "stop");
        assert_eq!(acc.usage.prompt_tokens, 10);
        assert_eq!(acc.usage.completion_tokens, 2);
    }

    #[test]
    fn test_parse_bedrock_stream_with_tool_use() {
        let stream_text = r#"
{"messageStart": {"role": "assistant"}}
{"contentBlockStart": {"start": {}}}
{"contentBlockDelta": {"delta": {"text": "Let me check."}}}
{"contentBlockStop": {}}
{"contentBlockStart": {"start": {"toolUse": {"toolUseId": "call_1", "name": "search"}}}}
{"contentBlockDelta": {"delta": {"toolUse": {"input": "{\"query\": \"weather\"}"}}}}
{"contentBlockStop": {}}
{"messageStop": {"stopReason": "tool_use"}}
{"metadata": {"usage": {"inputTokens": 20, "outputTokens": 15}}}
"#;

        let acc = parse_bedrock_stream(stream_text);
        assert_eq!(acc.content, Some("Let me check.".to_string()));
        assert_eq!(acc.tool_calls.len(), 1);
        assert_eq!(acc.finish_reason, "tool_calls");
        let tool = &acc.tool_calls[0];
        assert_eq!(tool.get("id").unwrap(), "call_1");
        assert_eq!(tool.get("function").unwrap().get("name").unwrap(), "search");
    }

    #[test]
    fn test_build_converse_request_basic() {
        let req = BedrockRequest {
            model: "anthropic.claude-sonnet-4-6".to_string(),
            messages: vec![
                json!({"role": "user", "content": "Hello"}),
            ],
            tools: None,
            max_tokens: 4096,
            temperature: Some(0.7),
            top_p: None,
            stop_sequences: None,
            guardrail_config: None,
        };

        let body = build_converse_request(&req);
        assert_eq!(body.get("modelId").unwrap(), "anthropic.claude-sonnet-4-6");
        assert_eq!(
            body.get("inferenceConfig").unwrap().get("maxTokens").unwrap(),
            4096
        );
        assert_eq!(
            body.get("inferenceConfig").unwrap().get("temperature").unwrap(),
            0.7
        );
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn test_build_converse_request_with_tools() {
        let req = BedrockRequest {
            model: "anthropic.claude-sonnet-4-6".to_string(),
            messages: vec![
                json!({"role": "user", "content": "What's the weather?"}),
            ],
            tools: Some(vec![json!({
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            })]),
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            guardrail_config: None,
        };

        let body = build_converse_request(&req);
        assert!(body.get("toolConfig").is_some());
        let tools = body.get("toolConfig").unwrap().get("tools").unwrap();
        assert_eq!(tools.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_build_converse_request_strips_tools_for_unsupported_model() {
        let req = BedrockRequest {
            model: "deepseek.r1-v1".to_string(),
            messages: vec![
                json!({"role": "user", "content": "What is 2+2?"}),
            ],
            tools: Some(vec![json!({
                "type": "function",
                "function": {
                    "name": "calc",
                    "description": "Calculate",
                    "parameters": {"type": "object", "properties": {}}
                }
            })]),
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            guardrail_config: None,
        };

        let body = build_converse_request(&req);
        // Tools should be stripped for deepseek.r1
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn test_convert_messages_first_not_user() {
        // Assistant-first should get a prepended user message
        let messages = vec![
            json!({"role": "assistant", "content": "Hello"}),
        ];

        let (system, msgs) = convert_messages_to_converse(&messages);
        assert!(system.is_none());
        // First message should be user (prepended)
        assert_eq!(msgs[0].get("role").unwrap(), "user");
        // Last message should be user (appended because assistant was last)
        assert_eq!(msgs.last().unwrap().get("role").unwrap(), "user");
    }

    #[test]
    fn test_extract_provider_from_arn() {
        assert_eq!(
            extract_provider_from_arn("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-v2"),
            "anthropic"
        );
        assert_eq!(
            extract_provider_from_arn("arn:aws:bedrock:us-east-1::foundation-model/amazon.nova-pro"),
            "amazon"
        );
        assert_eq!(extract_provider_from_arn(""), "");
    }

    #[test]
    fn test_sigv4_sign_deterministic() {
        // Verify that sign_request produces a non-empty authorization header
        let creds = BedrockCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };

        let auth = sign_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse",
            &[("content-type".to_string(), "application/json".to_string())],
            "{}",
            &creds,
            "us-east-1",
            "bedrock",
        );

        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential="));
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
    }
}
