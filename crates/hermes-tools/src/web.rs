#![allow(dead_code)]
//! Web search and URL extraction tools.
//!
//! Mirrors the Python `tools/web_tools.py`.
//! Pluggable backends: Exa, Firecrawl, Tavily. Configurable via env vars.

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

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
    [SearchBackend::Exa, SearchBackend::Tavily, SearchBackend::Firecrawl].into_iter().find(|&backend| std::env::var(backend.api_key_env()).is_ok())
}

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

/// Strip HTML tags from a string (simple regex-based approach).
fn strip_html_tags(html: &str) -> String {
    // Simple approach: remove anything between < and >
    // This is not a full HTML parser but sufficient for text extraction
    static STRIP_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]*>").unwrap());
    let text = STRIP_RE.replace_all(html, "").to_string();
    // Normalize whitespace
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

    // Run async search in a sync context using tokio block_on
    // This is a simplification; in production, the tool dispatcher should be async
    let rt = tokio::runtime::Handle::current();
    let result = match rt.block_on(web_search(&query, num_results, backend)) {
        Ok(r) => r,
        Err(e) => tool_error(&e),
    };

    Ok(result)
}

/// Handler for web_extract tool (sync wrapper).
pub fn handle_web_extract(args: Value) -> Result<String, hermes_core::HermesError> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "web_extract requires 'url' parameter",
            )
        })?
        .to_string();

    let rt = tokio::runtime::Handle::current();
    let result = match rt.block_on(web_extract_url(&url)) {
        Ok(r) => r,
        Err(e) => tool_error(&e),
    };

    Ok(result)
}

/// Register web search tools.
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
        "description": "Extract text content from a URL. Strips HTML tags and returns readable text. Useful for reading articles, documentation, or any web page content. Truncated at 50K characters.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to extract content from" }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_resolve_backend_no_keys() {
        // In test environment, no API keys should be set
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
        // "hello " is 6 bytes, "世" starts at byte 6
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
        assert!(result.is_err());
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
        // Should fail because no backend configured
        assert!(result.is_ok()); // Returns error JSON, not Err
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }
}
