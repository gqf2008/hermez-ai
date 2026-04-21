#![allow(dead_code)]
//! Web search, extract, and crawl tools.
//!
//! Mirrors the Python `tools/web_tools.py`.
//! Pluggable backends: Exa, Firecrawl, Tavily. Configurable via env vars.
//!
//! Priority features vs Python:
//! - Web crawl (Firecrawl v2 crawl API, Tavily crawl)
//! - LLM processing (chunked summarization, synthesis, auxiliary model)
//! - SSRF protection (secret exfiltration guard, website policy)

use regex::Regex;
use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

// ─── Secret exfiltration guard ──────────────────────────────────────────────

/// Known API key / token prefix patterns for SSRF secret detection.
/// Mirrors Python `agent/redact.py` `_PREFIX_PATTERNS`.
const PREFIX_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{10,}",
    r"ghp_[A-Za-z0-9]{10,}",
    r"github_pat_[A-Za-z0-9_]{10,}",
    r"gho_[A-Za-z0-9]{10,}",
    r"ghu_[A-Za-z0-9]{10,}",
    r"ghs_[A-Za-z0-9]{10,}",
    r"ghr_[A-Za-z0-9]{10,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AIza[A-Za-z0-9_-]{30,}",
    r"pplx-[A-Za-z0-9]{10,}",
    r"fal_[A-Za-z0-9_-]{10,}",
    r"fc-[A-Za-z0-9]{10,}",
    r"bb_live_[A-Za-z0-9_-]{10,}",
    r"gAAAA[A-Za-z0-9_=-]{20,}",
    r"AKIA[A-Z0-9]{16}",
    r"sk_live_[A-Za-z0-9]{10,}",
    r"sk_test_[A-Za-z0-9]{10,}",
    r"rk_live_[A-Za-z0-9]{10,}",
    r"SG\.[A-Za-z0-9_-]{10,}",
    r"hf_[A-Za-z0-9]{10,}",
    r"r8_[A-Za-z0-9]{10,}",
    r"npm_[A-Za-z0-9]{10,}",
    r"pypi-[A-Za-z0-9_-]{10,}",
    r"dop_v1_[A-Za-z0-9]{10,}",
    r"doo_v1_[A-Za-z0-9]{10,}",
    r"am_[A-Za-z0-9_-]{10,}",
    r"sk_[A-Za-z0-9_]{10,}",
    r"tvly-[A-Za-z0-9]{10,}",
    r"exa_[A-Za-z0-9]{10,}",
    r"gsk_[A-Za-z0-9]{10,}",
    r"syt_[A-Za-z0-9]{10,}",
    r"retaindb_[A-Za-z0-9]{10,}",
    r"hsk-[A-Za-z0-9]{10,}",
    r"mem0_[A-Za-z0-9]{10,}",
    r"brv_[A-Za-z0-9]{10,}",
];

/// Compiled regex that detects secrets in URLs.
/// Mirrors Python `agent/redact.py` `_PREFIX_RE`, but without look-around
/// assertions (not supported by the Rust regex crate).
static SECRET_PREFIX_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    let alternation = PREFIX_PATTERNS.join("|");
    Regex::new(&alternation).unwrap()
});

/// Check whether a character is a word character for secret boundary checking.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Check whether the match at `[start, end)` in `text` has word boundaries on both sides.
fn has_word_boundaries(text: &str, start: usize, end: usize) -> bool {
    let left_ok = start == 0 || text[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
    let right_ok = end >= text.len() || text[end..].chars().next().is_none_or(|c| !is_word_char(c));
    left_ok && right_ok
}

/// Check whether a URL (or its percent-decoded form) contains an embedded secret.
/// Returns the matching secret fragment if found, else `None`.
fn check_url_for_secrets(url: &str) -> Option<String> {
    for m in SECRET_PREFIX_RE.find_iter(url) {
        if has_word_boundaries(url, m.start(), m.end()) {
            return Some(m.as_str().to_string());
        }
    }
    // URL-decode first so percent-encoded secrets (%73k- = sk-) are caught.
    let decoded = percent_decode(url);
    for m in SECRET_PREFIX_RE.find_iter(&decoded) {
        if has_word_boundaries(&decoded, m.start(), m.end()) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

/// Simple percent-decoding for secret detection.
/// Does NOT handle Unicode beyond ASCII (sufficient for secret prefixes).
fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                result.push((h * 16 + l) as char);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ─── Backend provider ───────────────────────────────────────────────────────

/// Backend provider for web search.
#[derive(Debug, Clone, Copy)]
pub enum SearchBackend {
    Exa,
    Firecrawl,
    Tavily,
}

impl SearchBackend {
    pub fn parse(s: &str) -> Self {
        match s {
            "exa" => SearchBackend::Exa,
            "firecrawl" => SearchBackend::Firecrawl,
            "tavily" => SearchBackend::Tavily,
            _ => SearchBackend::Exa, // default
        }
    }

    /// Get the API key env var name for this backend.
    pub fn api_key_env(&self) -> &'static str {
        match self {
            SearchBackend::Exa => "EXA_API_KEY",
            SearchBackend::Firecrawl => "FIRECRAWL_API_KEY",
            SearchBackend::Tavily => "TAVILY_API_KEY",
        }
    }

    /// Get the API base URL for this backend.
    pub fn api_base(&self) -> &'static str {
        match self {
            SearchBackend::Exa => "https://api.exa.ai",
            SearchBackend::Firecrawl => "https://api.firecrawl.dev",
            SearchBackend::Tavily => "https://api.tavily.com",
        }
    }
}

/// Resolve the best available backend based on available API keys.
pub fn resolve_backend() -> Option<SearchBackend> {
    // Try each backend in priority order
    [SearchBackend::Exa, SearchBackend::Tavily, SearchBackend::Firecrawl]
        .into_iter()
        .find(|&backend| std::env::var(backend.api_key_env()).is_ok())
}

// ─── Web search ─────────────────────────────────────────────────────────────

/// Search the web for a query.
///
/// Returns search results as JSON string with title, URL, and snippet.
pub async fn web_search(query: &str, num_results: usize, backend: SearchBackend) -> Result<String, String> {
    let api_key = std::env::var(backend.api_key_env())
        .map_err(|_| format!("{} not set", backend.api_key_env()))?;

    let client = reqwest::Client::new();

    let (url, body) = match backend {
        SearchBackend::Exa => (
            format!("{}/search", backend.api_base()),
            serde_json::json!({
                "query": query,
                "numResults": num_results,
                "type": "neural",
                "useAutoprompt": true,
            }),
        ),
        SearchBackend::Tavily => (
            format!("{}/search", backend.api_base()),
            serde_json::json!({
                "api_key": api_key,
                "query": query,
                "max_results": num_results,
                "search_depth": "basic",
                "include_answer": true,
            }),
        ),
        SearchBackend::Firecrawl => (
            format!("{}/v1/search", backend.api_base()),
            serde_json::json!({
                "query": query,
                "limit": num_results,
            }),
        ),
    };

    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("API returned status {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    // Normalize results to a common format
    let results = match backend {
        SearchBackend::Exa => {
            json.get("results")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|r| serde_json::json!({
                            "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                            "snippet": r.get("text").and_then(Value::as_str).unwrap_or(""),
                        }))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
        SearchBackend::Tavily => {
            json.get("results")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|r| serde_json::json!({
                            "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                            "snippet": r.get("content").and_then(Value::as_str).unwrap_or(""),
                        }))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
        SearchBackend::Firecrawl => {
            json.get("data")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|r| serde_json::json!({
                            "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                            "snippet": r.get("description").and_then(Value::as_str).unwrap_or(""),
                        }))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
    };

    Ok(serde_json::json!({
        "query": query,
        "backend": format!("{:?}", backend),
        "results": results,
        "total_results": results.len(),
    })
    .to_string())
}

// ─── Web extract ────────────────────────────────────────────────────────────

/// Extract content from a URL.
pub async fn web_extract_url(url: &str) -> Result<String, String> {
    // SSRF prevention: block private/internal URLs
    if !crate::url_safety::is_safe_url(url) {
        return Err(format!("URL blocked by SSRF filter: {url}"));
    }

    let client = reqwest::Client::new();

    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (compatible; HermesAgent/1.0)")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch URL: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, url));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    // Basic extraction: strip HTML tags, get text content
    let text = strip_html_tags(&html);
    let truncated = if text.len() > 50_000 {
        safe_truncate(&text, 50_000)
    } else {
        text
    };

    Ok(serde_json::json!({
        "url": url,
        "content": truncated,
        "original_length": html.len(),
        "extracted_length": truncated.len(),
    })
    .to_string())
}

// ─── LLM content processing ─────────────────────────────────────────────────

/// Default minimum content length to trigger LLM summarization.
const DEFAULT_MIN_LENGTH_FOR_SUMMARIZATION: usize = 5_000;

/// Maximum content size: refuse entirely above this (2M chars).
const MAX_CONTENT_SIZE: usize = 2_000_000;
/// Chunk threshold: use chunked processing above this (500k chars).
const CHUNK_THRESHOLD: usize = 500_000;
/// Chunk size for large content (~100k chars).
const CHUNK_SIZE: usize = 100_000;
/// Hard cap on final output size.
const MAX_OUTPUT_SIZE: usize = 5_000;

/// Process web content using an auxiliary LLM to create intelligent summaries.
///
/// Mirrors Python `process_content_with_llm`.
/// - For very large content (>500k chars), uses chunked processing with synthesis.
/// - For extremely large content (>2M chars), refuses to process entirely.
/// - On failure, falls back to truncated raw content.
pub async fn process_content_with_llm(
    content: &str,
    url: &str,
    title: &str,
    model: Option<&str>,
    min_length: usize,
) -> Option<String> {
    let content_len = content.len();

    // Refuse if content is absurdly large
    if content_len > MAX_CONTENT_SIZE {
        let size_mb = content_len as f64 / 1_000_000.0;
        tracing::warn!("Content too large ({:.1}MB > 2MB limit). Refusing to process.", size_mb);
        return Some(format!(
            "[Content too large to process: {:.1}MB. Try using web_crawl with specific extraction instructions, or search for a more focused source.]",
            size_mb
        ));
    }

    // Skip processing if content is too short
    if content_len < min_length {
        tracing::debug!("Content too short ({} < {} chars), skipping LLM processing", content_len, min_length);
        return None;
    }

    // Build context info
    let context_str = {
        let mut parts = Vec::new();
        if !title.is_empty() {
            parts.push(format!("Title: {title}"));
        }
        if !url.is_empty() {
            parts.push(format!("Source: {url}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            parts.join("\n") + "\n\n"
        }
    };

    // Chunked processing for large content
    if content_len > CHUNK_THRESHOLD {
        tracing::info!("Content large ({} chars). Using chunked processing...", content_len);
        return process_large_content_chunked(content, &context_str, model).await;
    }

    // Standard single-pass processing
    tracing::info!("Processing content with LLM ({} characters)", content_len);

    match call_summarizer_llm(content, &context_str, model, 20_000, false, "").await {
        Some(mut processed) => {
            if processed.len() > MAX_OUTPUT_SIZE {
                processed = safe_truncate(&processed, MAX_OUTPUT_SIZE)
                    + "\n\n[... summary truncated for context management ...]";
            }
            let processed_length = processed.len();
            let compression_ratio = processed_length as f64 / content_len as f64;
            tracing::info!(
                "Content processed: {} -> {} chars ({:.1}%)",
                content_len, processed_length, compression_ratio * 100.0
            );
            Some(processed)
        }
        None => None,
    }
}

/// Make a single LLM call to summarize content.
async fn call_summarizer_llm(
    content: &str,
    context_str: &str,
    model: Option<&str>,
    max_tokens: usize,
    is_chunk: bool,
    chunk_info: &str,
) -> Option<String> {
    let (system_prompt, user_prompt) = if is_chunk {
        let sys = "You are an expert content analyst processing a SECTION of a larger document. Your job is to extract and summarize the key information from THIS SECTION ONLY.

Important guidelines for chunk processing:
1. Do NOT write introductions or conclusions - this is a partial document
2. Focus on extracting ALL key facts, figures, data points, and insights from this section
3. Preserve important quotes, code snippets, and specific details verbatim
4. Use bullet points and structured formatting for easy synthesis later
5. Note any references to other sections (e.g., \"as mentioned earlier\", \"see below\") without trying to resolve them

Your output will be combined with summaries of other sections, so focus on thorough extraction rather than narrative flow.";
        let usr = format!(
            "{}Extract key information from this SECTION of a larger document:\n\n{}\n\nSECTION CONTENT:\n{}\n\nExtract all important information from this section in a structured format. Focus on facts, data, insights, and key details. Do not add introductions or conclusions.",
            context_str, chunk_info, content
        );
        (sys.to_string(), usr)
    } else {
        let sys = "You are an expert content analyst. Your job is to process web content and create a comprehensive yet concise summary that preserves all important information while dramatically reducing bulk.

Create a well-structured markdown summary that includes:
1. Key excerpts (quotes, code snippets, important facts) in their original format
2. Comprehensive summary of all other important information
3. Proper markdown formatting with headers, bullets, and emphasis

Your goal is to preserve ALL important information while reducing length. Never lose key facts, figures, insights, or actionable information. Make it scannable and well-organized.";
        let usr = format!(
            "{}Please process this web content and create a comprehensive markdown summary:\n\nCONTENT TO PROCESS:\n{}\n\nCreate a markdown summary that captures all key information in a well-organized, scannable format. Include important quotes and code snippets in their original formatting. Focus on actionable information, specific details, and unique insights.",
            context_str, content
        );
        (sys.to_string(), usr)
    };

    // Resolve model: explicit > config > default
    let effective_model = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AUXILIARY_WEB_EXTRACT_MODEL").ok().filter(|s| !s.is_empty()))
        .or_else(|| hermes_llm::auxiliary_client::get_default_aux_model("openrouter"))
        .unwrap_or_else(|| "google/gemini-3-flash-preview".to_string());

    let request = hermes_llm::auxiliary_client::AuxiliaryRequest {
        task: "web_extract".to_string(),
        provider: None,
        model: Some(effective_model),
        base_url: None,
        api_key: None,
        messages: vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": user_prompt}),
        ],
        temperature: Some(0.1),
        max_tokens: Some(max_tokens),
        tools: None,
        timeout_secs: None,
        extra_body: None,
    };

    match hermes_llm::auxiliary_client::call_auxiliary(request).await {
        Ok(response) => {
            let text = response.content;
            if text.is_empty() {
                tracing::warn!("LLM returned empty content during summarization");
                None
            } else {
                Some(text)
            }
        }
        Err(e) => {
            tracing::warn!("LLM summarization failed: {}", e);
            None
        }
    }
}

/// Process large content by chunking, summarizing each chunk in parallel,
/// then synthesizing the summaries.
async fn process_large_content_chunked(
    content: &str,
    context_str: &str,
    model: Option<&str>,
) -> Option<String> {
    // Split content into owned chunks
    let chunks: Vec<String> = content
        .chars()
        .collect::<Vec<_>>()
        .chunks(CHUNK_SIZE)
        .map(|c| c.iter().collect::<String>())
        .collect();

    let n_chunks = chunks.len();
    tracing::info!("Split into {} chunks of ~{} chars each", n_chunks, CHUNK_SIZE);

    // Summarize each chunk concurrently
    let mut tasks = Vec::new();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let chunk_info = format!("[Processing chunk {} of {}]", idx + 1, n_chunks);
        let ctx = context_str.to_string();
        let m = model.map(|s| s.to_string());
        tasks.push(tokio::spawn(async move {
            let summary = call_summarizer_llm(&chunk, &ctx, m.as_deref(), 10_000, true, &chunk_info).await;
            (idx, summary)
        }));
    }

    let mut summaries: Vec<(usize, String)> = Vec::new();
    for task in tasks {
        match task.await {
            Ok((idx, Some(summary))) => {
                tracing::info!("Chunk {}/{} summarized: {} chars", idx + 1, n_chunks, summary.len());
                summaries.push((idx, summary));
            }
            Ok((idx, None)) => {
                tracing::warn!("Chunk {}/{} failed summarization", idx + 1, n_chunks);
            }
            Err(e) => {
                tracing::warn!("Chunk task panicked: {}", e);
            }
        }
    }

    summaries.sort_by_key(|(idx, _)| *idx);

    if summaries.is_empty() {
        return Some("[Failed to process large content: all chunk summarizations failed]".to_string());
    }

    let summary_sections: Vec<String> = summaries
        .iter()
        .enumerate()
        .map(|(i, (_, s))| format!("## Section {}\n{}", i + 1, s))
        .collect();

    // If only one chunk succeeded, return it directly
    if summary_sections.len() == 1 {
        let mut result = summary_sections.into_iter().next().unwrap();
        if result.len() > MAX_OUTPUT_SIZE {
            result = safe_truncate(&result, MAX_OUTPUT_SIZE) + "\n\n[... truncated ...]";
        }
        return Some(result);
    }

    // Synthesize summaries into a final summary
    tracing::info!("Synthesizing {} summaries...", summary_sections.len());
    let combined = summary_sections.join("\n\n---\n\n");

    let synthesis_prompt = format!(
        "You have been given summaries of different sections of a large document.\n\
         Synthesize these into ONE cohesive, comprehensive summary that:\n\
         1. Removes redundancy between sections\n\
         2. Preserves all key facts, figures, and actionable information\n\
         3. Is well-organized with clear structure\n\
         4. Is under {} characters\n\n\
         {}SECTION SUMMARIES:\n{}\n\n\
         Create a single, unified markdown summary.",
        MAX_OUTPUT_SIZE, context_str, combined
    );

    let effective_model = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AUXILIARY_WEB_EXTRACT_MODEL").ok().filter(|s| !s.is_empty()))
        .or_else(|| hermes_llm::auxiliary_client::get_default_aux_model("openrouter"))
        .unwrap_or_else(|| "google/gemini-3-flash-preview".to_string());

    let request = hermes_llm::auxiliary_client::AuxiliaryRequest {
        task: "web_extract".to_string(),
        provider: None,
        model: Some(effective_model),
        base_url: None,
        api_key: None,
        messages: vec![
            serde_json::json!({"role": "system", "content": "You synthesize multiple summaries into one cohesive, comprehensive summary. Be thorough but concise."}),
            serde_json::json!({"role": "user", "content": synthesis_prompt}),
        ],
        temperature: Some(0.1),
        max_tokens: Some(20_000),
        tools: None,
        timeout_secs: None,
        extra_body: None,
    };

    let mut final_summary = match hermes_llm::auxiliary_client::call_auxiliary(request).await {
        Ok(response) => {
            let text = response.content;
            if text.is_empty() {
                // Fallback to concatenated summaries
                let fallback = summaries.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n\n");
                fallback
            } else {
                text
            }
        }
        Err(e) => {
            tracing::warn!("Synthesis failed: {}. Falling back to concatenated summaries.", e);
            summaries.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n\n")
        }
    };

    if final_summary.len() > MAX_OUTPUT_SIZE {
        final_summary = safe_truncate(&final_summary, MAX_OUTPUT_SIZE)
            + "\n\n[... summary truncated for context management ...]";
    }

    let original_len = content.len();
    let final_len = final_summary.len();
    let compression = final_len as f64 / original_len as f64;
    tracing::info!(
        "Synthesis complete: {} -> {} chars ({:.2}%)",
        original_len, final_len, compression * 100.0
    );

    Some(final_summary)
}

// ─── Web crawl ──────────────────────────────────────────────────────────────

/// Crawl a website with specific instructions.
///
/// Supports Firecrawl v2 crawl API and Tavily crawl.
/// Returns crawled pages as JSON with title, URL, and content per page.
pub async fn web_crawl(
    url: &str,
    _instructions: Option<&str>,
    depth: &str,
    backend: SearchBackend,
) -> Result<String, String> {
    // Ensure URL has protocol
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{}", url)
    };

    // SSRF protection
    if !crate::url_safety::is_safe_url(&url) {
        return Ok(serde_json::json!({
            "results": [{
                "url": url,
                "title": "",
                "content": "",
                "error": "Blocked: URL targets a private or internal network address"
            }]
        }).to_string());
    }

    // Secret exfiltration guard
    if let Some(secret) = check_url_for_secrets(&url) {
        return Ok(tool_error(format!(
            "Blocked: URL contains what appears to be an API key or token ({}). Secrets must not be sent in URLs.",
            secret
        )));
    }

    // Website policy check
    if let Some(blocked) = crate::website_policy::check_website_access(&url) {
        let message = blocked.get("message").cloned().unwrap_or_default();
        return Ok(serde_json::json!({
            "results": [{
                "url": url,
                "title": "",
                "content": "",
                "error": message,
                "blocked_by_policy": blocked
            }]
        }).to_string());
    }

    match backend {
        SearchBackend::Tavily => tavily_crawl(&url, depth).await,
        _ => firecrawl_crawl(&url, depth).await,
    }
}

/// Crawl using Tavily /crawl endpoint.
async fn tavily_crawl(url: &str, depth: &str) -> Result<String, String> {
    let api_key = std::env::var("TAVILY_API_KEY")
        .map_err(|_| "TAVILY_API_KEY not set".to_string())?;

    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "api_key": api_key,
        "url": url,
        "limit": 20,
        "extract_depth": depth,
    });

    let resp = client
        .post("https://api.tavily.com/crawl")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Tavily crawl request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("Tavily crawl API returned status {}: {}", status, resp.text().await.unwrap_or_default()));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Tavily crawl response: {}", e))?;

    let mut results = Vec::new();
    for result in json.get("results").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        let page_url = result.get("url").and_then(Value::as_str).unwrap_or(url);
        let raw = result.get("raw_content").and_then(Value::as_str)
            .or_else(|| result.get("content").and_then(Value::as_str))
            .unwrap_or("");
        results.push(serde_json::json!({
            "url": page_url,
            "title": result.get("title").and_then(Value::as_str).unwrap_or(""),
            "content": raw,
            "raw_content": raw,
            "metadata": {
                "sourceURL": page_url,
                "title": result.get("title").and_then(Value::as_str).unwrap_or("")
            }
        }));
    }

    // Handle failed results
    for fail in json.get("failed_results").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        results.push(serde_json::json!({
            "url": fail.get("url").and_then(Value::as_str).unwrap_or(url),
            "title": "",
            "content": "",
            "raw_content": "",
            "error": fail.get("error").and_then(Value::as_str).unwrap_or("extraction failed"),
        }));
    }
    for fail_url in json.get("failed_urls").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        let url_str = fail_url.as_str().map(|s| s.to_string()).unwrap_or_else(|| fail_url.to_string());
        results.push(serde_json::json!({
            "url": url_str,
            "title": "",
            "content": "",
            "raw_content": "",
            "error": "extraction failed",
        }));
    }

    Ok(serde_json::json!({ "results": results }).to_string())
}

/// Crawl using Firecrawl v2 crawl API.
async fn firecrawl_crawl(url: &str, _depth: &str) -> Result<String, String> {
    let api_key = std::env::var("FIRECRAWL_API_KEY")
        .map_err(|_| "FIRECRAWL_API_KEY not set".to_string())?;

    let client = reqwest::Client::new();

    // Initiate crawl job
    let crawl_params = serde_json::json!({
        "url": url,
        "limit": 20,
        "scrapeOptions": {
            "formats": ["markdown"]
        }
    });

    let init_resp = client
        .post("https://api.firecrawl.dev/v1/crawl")
        .bearer_auth(&api_key)
        .json(&crawl_params)
        .send()
        .await
        .map_err(|e| format!("Firecrawl crawl init failed: {}", e))?;

    let init_status = init_resp.status();
    if !init_status.is_success() {
        return Err(format!(
            "Firecrawl crawl init returned status {}: {}",
            init_status,
            init_resp.text().await.unwrap_or_default()
        ));
    }

    let init_json: Value = init_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Firecrawl crawl init: {}", e))?;

    let job_id = init_json
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Firecrawl crawl response missing job id".to_string())?;

    // Poll for completion
    let mut pages = Vec::new();
    let poll_url = format!("https://api.firecrawl.dev/v1/crawl/{}", job_id);
    let max_polls = 30;
    let poll_interval_secs = 2;

    for _ in 0..max_polls {
        tokio::time::sleep(tokio::time::Duration::from_secs(poll_interval_secs)).await;

        let status_resp = client
            .get(&poll_url)
            .bearer_auth(&api_key)
            .send()
            .await
            .map_err(|e| format!("Firecrawl crawl status poll failed: {}", e))?;

        let status_json: Value = status_resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse Firecrawl crawl status: {}", e))?;

        let status = status_json
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if let Some(data) = status_json.get("data").and_then(Value::as_array) {
            for item in data {
                let page_url = item
                    .get("metadata")
                    .and_then(|m| m.get("sourceURL"))
                    .and_then(Value::as_str)
                    .or_else(|| item.get("url").and_then(Value::as_str))
                    .unwrap_or(url);
                let title = item
                    .get("metadata")
                    .and_then(|m| m.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let content = item
                    .get("markdown")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                // Re-check crawled page against policy
                if crate::website_policy::check_website_access(page_url).is_some() {
                    continue; // Skip blocked pages
                }

                pages.push(serde_json::json!({
                    "url": page_url,
                    "title": title,
                    "content": content,
                    "raw_content": content,
                    "metadata": item.get("metadata").cloned().unwrap_or_else(|| serde_json::json!({}))
                }));
            }
        }

        if status == "completed" || status == "scraping" && pages.len() >= 20 {
            break;
        }
        if status == "failed" {
            let err = status_json
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Crawl failed");
            return Err(format!("Firecrawl crawl failed: {}", err));
        }
    }

    Ok(serde_json::json!({ "results": pages }).to_string())
}

// ─── HTML helpers ───────────────────────────────────────────────────────────

/// Strip HTML tags from a string (simple regex-based approach).
fn strip_html_tags(html: &str) -> String {
    static STRIP_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]*>").unwrap());
    let text = STRIP_RE.replace_all(html, "").to_string();
    static WS_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());
    WS_RE.replace_all(&text, " ").trim().to_string()
}

/// Safe UTF-8 truncation.
fn safe_truncate(text: &str, max_bytes: usize) -> String {
    if max_bytes >= text.len() {
        return text.to_string();
    }
    let mut safe = max_bytes;
    while safe > 0 && !text.is_char_boundary(safe) {
        safe -= 1;
    }
    text[..safe].to_string()
}

/// Remove base64 encoded images from text to reduce token count.
fn clean_base64_images(text: &str) -> String {
    static BASE64_PARENS_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\(data:image/[^;]+;base64,[A-Za-z0-9+/=]+\)").unwrap()
        });
    static BASE64_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r"data:image/[^;]+;base64,[A-Za-z0-9+/=]+").unwrap()
        });
    let cleaned = BASE64_PARENS_RE.replace_all(text, "[BASE64_IMAGE_REMOVED]");
    BASE64_RE.replace_all(&cleaned, "[BASE64_IMAGE_REMOVED]").to_string()
}

// ─── Tool handlers ──────────────────────────────────────────────────────────

/// Handler for web_search tool (sync wrapper).
pub fn handle_web_search(args: Value) -> Result<String, hermes_core::HermesError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "web_search requires 'query' parameter",
            )
        })?
        .to_string();

    let num_results = args.get("num_results").and_then(Value::as_u64).unwrap_or(5) as usize;
    let backend_str = args.get("backend").and_then(Value::as_str).unwrap_or("auto");

    let backend = if backend_str == "auto" {
        match resolve_backend() {
            Some(b) => b,
            None => return Ok(tool_error(
                "No web search backend configured. Set EXA_API_KEY, TAVILY_API_KEY, or FIRECRAWL_API_KEY.",
            )),
        }
    } else {
        SearchBackend::parse(backend_str)
    };

    let rt = tokio::runtime::Handle::current();
    let result = match rt.block_on(web_search(&query, num_results, backend)) {
        Ok(r) => r,
        Err(e) => tool_error(&e),
    };

    Ok(result)
}

/// Handler for web_extract tool (sync wrapper).
///
/// Enhanced vs MVP: SSRF secret guard, website policy checks, optional LLM processing.
pub fn handle_web_extract(args: Value) -> Result<String, hermes_core::HermesError> {
    let urls = args
        .get("url")
        .and_then(|v| {
            if let Some(arr) = v.as_array() {
                Some(arr.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
            } else {
                v.as_str().map(|s| vec![s.to_string()])
            }
        })
        .unwrap_or_default();

    if urls.is_empty() {
        return Ok(tool_error("web_extract requires 'url' parameter"));
    }

    // Secret exfiltration guard
    for url in &urls {
        if let Some(secret) = check_url_for_secrets(url) {
            return Ok(tool_error(format!(
                "Blocked: URL contains what appears to be an API key or token ({}). Secrets must not be sent in URLs.",
                secret
            )));
        }
    }

    let use_llm = args.get("use_llm_processing").and_then(Value::as_bool).unwrap_or(true);
    let model = args.get("model").and_then(Value::as_str).map(String::from);
    let min_length = args
        .get("min_length")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MIN_LENGTH_FOR_SUMMARIZATION as u64) as usize;

    let rt = tokio::runtime::Handle::current();
    let result = match rt.block_on(async {
        let mut results = Vec::new();
        for url in &urls {
            // SSRF filter
            if !crate::url_safety::is_safe_url(url) {
                results.push(serde_json::json!({
                    "url": url,
                    "title": "",
                    "content": "",
                    "error": "Blocked: URL targets a private or internal network address"
                }));
                continue;
            }

            // Website policy check
            if let Some(blocked) = crate::website_policy::check_website_access(url) {
                let message = blocked.get("message").cloned().unwrap_or_default();
                results.push(serde_json::json!({
                    "url": url,
                    "title": "",
                    "content": "",
                    "error": message,
                    "blocked_by_policy": blocked
                }));
                continue;
            }

            match web_extract_url(url).await {
                Ok(extracted_json) => {
                    let parsed: Value = serde_json::from_str(&extracted_json).unwrap_or(serde_json::json!({}));
                    let content = parsed.get("content").and_then(Value::as_str).unwrap_or("").to_string();
                    let title = parsed.get("title").and_then(Value::as_str).unwrap_or("").to_string();

                    // Optional LLM processing
                    let final_content = if use_llm {
                        process_content_with_llm(&content, url, &title, model.as_deref(), min_length).await
                    } else {
                        None
                    };

                    let raw_content = content.clone();
                    let effective_content = final_content.unwrap_or(content);

                    results.push(serde_json::json!({
                        "url": url,
                        "title": title,
                        "content": effective_content,
                        "raw_content": raw_content,
                    }));
                }
                Err(e) => {
                    results.push(serde_json::json!({
                        "url": url,
                        "title": "",
                        "content": "",
                        "error": e
                    }));
                }
            }
        }
        let response = serde_json::json!({ "results": results });
        let json_str = serde_json::to_string_pretty(&response).unwrap_or_default();
        Ok::<String, String>(clean_base64_images(&json_str))
    }) {
        Ok(r) => r,
        Err(e) => tool_error(&e),
    };

    Ok(result)
}

/// Handler for web_crawl tool (sync wrapper).
pub fn handle_web_crawl(args: Value) -> Result<String, hermes_core::HermesError> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "web_crawl requires 'url' parameter",
            )
        })?
        .to_string();

    let instructions = args.get("instructions").and_then(Value::as_str);
    let depth = args.get("depth").and_then(Value::as_str).unwrap_or("basic");
    let backend_str = args.get("backend").and_then(Value::as_str).unwrap_or("auto");

    let backend = if backend_str == "auto" {
        match resolve_backend() {
            Some(b) => b,
            None => return Ok(tool_error(
                "No web backend configured. Set EXA_API_KEY, TAVILY_API_KEY, or FIRECRAWL_API_KEY.",
            )),
        }
    } else {
        SearchBackend::parse(backend_str)
    };

    let use_llm = args.get("use_llm_processing").and_then(Value::as_bool).unwrap_or(true);
    let model = args.get("model").and_then(Value::as_str).map(String::from);
    let min_length = args
        .get("min_length")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MIN_LENGTH_FOR_SUMMARIZATION as u64) as usize;

    let rt = tokio::runtime::Handle::current();
    let result = match rt.block_on(async {
        let crawl_result = web_crawl(&url, instructions, depth, backend).await?;
        let mut parsed: Value = serde_json::from_str(&crawl_result)
            .map_err(|e| format!("Failed to parse crawl result: {}", e))?;

        // Optional LLM processing per page
        if use_llm {
            if let Some(results) = parsed.get_mut("results").and_then(Value::as_array_mut) {
                for result in results.iter_mut() {
                    if let Some(obj) = result.as_object_mut() {
                        let page_url = obj.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                        let title = obj.get("title").and_then(Value::as_str).unwrap_or("").to_string();
                        let content = obj.get("content").and_then(Value::as_str).unwrap_or("").to_string();
                        if !content.is_empty() {
                            if let Some(processed) = process_content_with_llm(
                                &content, &page_url, &title, model.as_deref(), min_length
                            ).await {
                                obj.insert("raw_content".to_string(), Value::String(content));
                                obj.insert("content".to_string(), Value::String(processed));
                            }
                        }
                    }
                }
            }
        }

        let json_str = serde_json::to_string_pretty(&parsed).unwrap_or_default();
        Ok::<String, String>(clean_base64_images(&json_str))
    }) {
        Ok(r) => r,
        Err(e) => tool_error(&e),
    };

    Ok(result)
}

// ─── Registration ───────────────────────────────────────────────────────────

/// Register web search, extract, and crawl tools.
pub fn register_web_tools(registry: &mut ToolRegistry) {
    // web_search
    let search_schema = serde_json::json!({
        "name": "web_search",
        "description": "Search the web for information. Supports multiple backends (Exa, Tavily, Firecrawl). Returns title, URL, and snippet for each result. Use this instead of shell curl/wget for web research.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "num_results": { "type": "integer", "description": "Number of results to return (default: 5)", "default": 5, "minimum": 1, "maximum": 20 },
                "backend": { "type": "string", "enum": ["auto", "exa", "tavily", "firecrawl"], "description": "Search backend to use (default: auto-detect from available API keys)", "default": "auto" }
            },
            "required": ["query"]
        }
    });

    registry.register(
        "web_search".to_string(),
        "web".to_string(),
        search_schema,
        std::sync::Arc::new(handle_web_search),
        None,
        vec![],
        "Search the web for information".to_string(),
        "🔍".to_string(),
        None,
    );

    // web_extract
    let extract_schema = serde_json::json!({
        "name": "web_extract",
        "description": "Extract text content from one or more URLs. Strips HTML tags and returns readable text. Supports optional LLM summarization for long content. SSRF-protected and website-policy aware.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "oneOf": [
                        { "type": "string", "description": "URL to extract content from" },
                        { "type": "array", "items": { "type": "string" }, "description": "List of URLs to extract" }
                    ],
                    "description": "URL(s) to extract content from"
                },
                "use_llm_processing": { "type": "boolean", "description": "Whether to process content with LLM for summarization (default: true)", "default": true },
                "model": { "type": "string", "description": "Model to use for LLM processing (defaults to auxiliary backend model)" },
                "min_length": { "type": "integer", "description": "Minimum content length to trigger LLM processing (default: 5000)", "default": 5000 }
            },
            "required": ["url"]
        }
    });

    registry.register(
        "web_extract".to_string(),
        "web".to_string(),
        extract_schema,
        std::sync::Arc::new(handle_web_extract),
        None,
        vec![],
        "Extract text content from a URL".to_string(),
        "📄".to_string(),
        Some(50_000),
    );

    // web_crawl
    let crawl_schema = serde_json::json!({
        "name": "web_crawl",
        "description": "Crawl a website and extract content from multiple linked pages. Supports Firecrawl and Tavily backends. Optional LLM processing per page.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Base URL to crawl (can include or exclude https://)" },
                "instructions": { "type": "string", "description": "Instructions for what to crawl/extract (optional)" },
                "depth": { "type": "string", "enum": ["basic", "advanced"], "description": "Depth of extraction (default: basic)", "default": "basic" },
                "backend": { "type": "string", "enum": ["auto", "firecrawl", "tavily"], "description": "Crawl backend to use (default: auto-detect)", "default": "auto" },
                "use_llm_processing": { "type": "boolean", "description": "Whether to process each page with LLM for summarization (default: true)", "default": true },
                "model": { "type": "string", "description": "Model to use for LLM processing (defaults to auxiliary backend model)" },
                "min_length": { "type": "integer", "description": "Minimum content length to trigger LLM processing (default: 5000)", "default": 5000 }
            },
            "required": ["url"]
        }
    });

    registry.register(
        "web_crawl".to_string(),
        "web".to_string(),
        crawl_schema,
        std::sync::Arc::new(handle_web_crawl),
        None,
        vec![],
        "Crawl a website and extract content from multiple pages".to_string(),
        "🕷️".to_string(),
        Some(100_000),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_resolve_backend_no_keys() {
        std::env::remove_var("EXA_API_KEY");
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("FIRECRAWL_API_KEY");
        assert!(resolve_backend().is_none());
    }

    #[test]
    #[serial]
    fn test_resolve_backend_exa() {
        std::env::set_var("EXA_API_KEY", "test_key");
        assert!(resolve_backend().is_some());
        std::env::remove_var("EXA_API_KEY");
    }

    #[test]
    fn test_backend_api_key_env() {
        assert_eq!(SearchBackend::Exa.api_key_env(), "EXA_API_KEY");
        assert_eq!(SearchBackend::Tavily.api_key_env(), "TAVILY_API_KEY");
        assert_eq!(SearchBackend::Firecrawl.api_key_env(), "FIRECRAWL_API_KEY");
    }

    #[test]
    fn test_backend_api_base() {
        assert_eq!(SearchBackend::Exa.api_base(), "https://api.exa.ai");
        assert_eq!(SearchBackend::Tavily.api_base(), "https://api.tavily.com");
        assert_eq!(SearchBackend::Firecrawl.api_base(), "https://api.firecrawl.dev");
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("<p>Hello</p>"), "Hello");
        assert_eq!(strip_html_tags("<div><p>Test</p></div>"), "Test");
        assert_eq!(strip_html_tags("No tags here"), "No tags here");
        assert_eq!(strip_html_tags("<a href='x'>link</a>"), "link");
    }

    #[test]
    fn test_strip_html_complex() {
        let html = "<html><body><h1>Title</h1><p>Some <b>bold</b> text</p></body></html>";
        let text = strip_html_tags(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Some"));
        assert!(text.contains("bold"));
        assert!(text.contains("text"));
    }

    #[test]
    fn test_safe_truncate() {
        let text = "hello world";
        assert_eq!(safe_truncate(text, 100), "hello world");
        assert_eq!(safe_truncate(text, 5), "hello");
    }

    #[test]
    fn test_safe_truncate_utf8_boundary() {
        let text = "hello 世界";
        assert_eq!(safe_truncate(text, 7), "hello ");
        assert_eq!(safe_truncate(text, 9), "hello 世");
    }

    #[test]
    fn test_handler_search_missing_query() {
        let result = handle_web_search(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_extract_missing_url() {
        let result = handle_web_extract(serde_json::json!({}));
        assert!(result.is_ok()); // Returns error JSON
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_handler_search_no_backend() {
        std::env::remove_var("EXA_API_KEY");
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("FIRECRAWL_API_KEY");
        let result = handle_web_search(serde_json::json!({
            "query": "test"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_secret_prefix_detection() {
        assert!(check_url_for_secrets("https://evil.com?key=sk-abc1234567890").is_some());
        assert!(check_url_for_secrets("https://evil.com?key=AKIAIOSFODNN7EXAMPLE").is_some());
        assert!(check_url_for_secrets("https://example.com/page").is_none());
    }

    #[test]
    fn test_secret_prefix_percent_encoded() {
        // Percent-encoded secret prefix
        let url = "https://evil.com?key=sk%2Dabc1234567890";
        assert!(check_url_for_secrets(url).is_some());
    }

    #[test]
    fn test_clean_base64_images() {
        let text = "Here is an image: data:image/png;base64,iVBORw0KGgo= and more text";
        let cleaned = clean_base64_images(text);
        assert!(cleaned.contains("[BASE64_IMAGE_REMOVED]"));
        assert!(!cleaned.contains("iVBORw0KGgo"));
    }
}
