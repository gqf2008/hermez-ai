#![allow(dead_code)]
//! Vision analysis tool.
//!
//! Mirrors the Python `tools/vision_tools.py`.
//! 1 tool: vision_analyze — download image, base64 encode, call auxiliary LLM.
//! Security: SSRF prevention via URL safety check, redirect guard.

use std::path::PathBuf;

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};
use crate::url_safety;

/// Maximum base64-encoded image size (20 MB).
///
/// Mirrors Python: `_MAX_BASE64_BYTES = 20 * 1024 * 1024`.
/// Images larger than this are scaled down before sending to the API.
const MAX_BASE64_BYTES: usize = 20 * 1024 * 1024;

/// Target size after scaling down (5 MB).
///
/// When the API rejects an image as too large, retry at this size.
const TARGET_BASE64_BYTES: usize = 5 * 1024 * 1024;

/// Check if vision requirements are met (auxiliary provider available).
pub fn check_vision_requirements() -> bool {
    // At least one auxiliary provider key should be available
    std::env::var("OPENROUTER_API_KEY").is_ok()
        || std::env::var("NOUS_API_KEY").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
        || std::env::var("CUSTOM_LLM_URL").is_ok()
}

/// Validate image URL format.
fn validate_image_url(url: &str) -> bool {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return false;
    }
    match url::Url::parse(url) {
        Ok(parsed) => !parsed.host_str().unwrap_or("").is_empty(),
        Err(_) => false,
    }
}

/// Detect MIME type from magic bytes.
fn detect_mime_type(path: &std::path::Path) -> Option<&'static str> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 4 {
        return None;
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if data.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if data.starts_with(b"BM") {
        return Some("image/bmp");
    }
    // WebP: RIFF....WEBP
    if data.len() >= 12 && data.starts_with(b"RIFF") && data[8..12] == *b"WEBP" {
        return Some("image/webp");
    }
    // Fallback to extension
    match path.extension().and_then(|e| e.to_str()) {
        Some("svg") => Some("image/svg+xml"),
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("bmp") => Some("image/bmp"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Download image from URL to a temp file.
async fn download_image(url: &str, dest: &PathBuf) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (compatible; HermesAgent/1.0)")
        .header("Accept", "image/*,*/*;q=0.8")
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}: {url}"));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    std::fs::write(dest, &bytes)
        .map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(())
}

/// Encode image to base64 data URL.
fn image_to_data_url(path: &std::path::Path) -> Result<String, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("Failed to read image: {e}"))?;

    let mime_type = detect_mime_type(path).unwrap_or("image/jpeg");
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);

    Ok(format!("data:{mime_type};base64,{encoded}"))
}

/// Resize an image file to fit within a target base64 size.
///
/// Scales down the image progressively (halving dimensions each step)
/// until the base64-encoded output fits within `target_bytes`.
/// Uses JPEG encoding for photos, PNG for images with alpha.
fn resize_image_to_data_url(
    path: &std::path::Path,
    target_bytes: usize,
) -> Result<String, String> {
    let img = image::open(path)
        .map_err(|e| format!("Failed to decode image for resize: {e}"))?;

    // Convert RGBA to RGB for JPEG encoding (JPEG doesn't support alpha)
    let mime_type = detect_mime_type(path).unwrap_or("image/jpeg");
    let img = if mime_type != "image/png" && img.color().has_alpha() {
        image::DynamicImage::ImageRgba8(img.to_rgba8()).to_rgb8().into()
    } else {
        img
    };

    let mut width = img.width();
    let mut height = img.height();
    let mut encoded = String::new();

    for _ in 0..10 {
        // Resize if still too large
        let resized = if width < img.width() || height < img.height() {
            img.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
        } else {
            img.clone()
        };

        // Encode to bytes
        let mut buf = Vec::new();
        if mime_type == "image/png" {
            resized
                .write_to(
                    &mut std::io::Cursor::new(&mut buf),
                    image::ImageFormat::Png,
                )
                .map_err(|e| format!("Failed to encode PNG: {e}"))?;
        } else {
            // Default to JPEG at 85% quality
            resized
                .write_to(
                    &mut std::io::Cursor::new(&mut buf),
                    image::ImageFormat::Jpeg,
                )
                .map_err(|e| format!("Failed to encode JPEG: {e}"))?;
        }

        encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &buf,
        );

        if encoded.len() <= target_bytes {
            return Ok(format!("data:{mime_type};base64,{encoded}"));
        }

        // Halve dimensions for next iteration
        width = (width / 2).max(64);
        height = (height / 2).max(64);
    }

    // Final attempt at minimum size
    Ok(format!("data:{mime_type};base64,{encoded}"))
}

/// Call auxiliary LLM with vision prompt.
///
/// If the API rejects the image as too large, automatically rescales
/// and retries (mirrors Python vision auto-resize logic).
async fn call_vision_llm(
    image_data_url: &str,
    prompt: &str,
    model: Option<&str>,
    _image_path: Option<&std::path::Path>,
) -> Result<String, String> {
    use hermes_llm::auxiliary_client::{call_auxiliary, AuxiliaryRequest, AuxiliaryResponse};

    let messages = vec![serde_json::json!({
        "role": "user",
        "content": [
            {
                "type": "text",
                "text": prompt,
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": image_data_url,
                },
            },
        ],
    })];

    let request = AuxiliaryRequest {
        task: "vision".to_string(),
        provider: None,
        model: model.map(String::from),
        base_url: None,
        api_key: None,
        messages,
        temperature: Some(0.1),
        max_tokens: Some(2000),
        tools: None,
        timeout_secs: Some(120),
        extra_body: None,
    };

    let response: AuxiliaryResponse = call_auxiliary(request.clone())
        .await
        .map_err(|e| format!("Vision LLM error: {e:?}"))?;

    if response.content.is_empty() {
        // Retry once on empty content
        let retry: AuxiliaryResponse = call_auxiliary(request.clone())
            .await
            .map_err(|e| format!("Vision LLM error (retry): {e:?}"))?;
        if !retry.content.is_empty() {
            return Ok(retry.content);
        }
        return Err("Vision model returned empty response after retry".to_string());
    }

    Ok(response.content)
}

/// Call auxiliary LLM with vision prompt, auto-resizing on API rejection.
///
/// When the API rejects the image as too large (payload_too_large or
/// context_overflow), automatically rescales and retries.
async fn call_vision_llm_with_resize(
    image_data_url: &str,
    prompt: &str,
    model: Option<&str>,
    image_path: &std::path::Path,
) -> Result<String, String> {
    // Check if the encoded image exceeds the max size
    let b64_len = image_data_url.len().saturating_sub(
        image_data_url.find(";base64,").map_or(0, |i| i + 8),
    );

    if b64_len > MAX_BASE64_BYTES {
        tracing::warn!(
            "Vision image too large ({:.1} MB > {:.1} MB) — pre-scaling",
            b64_len as f64 / 1_048_576.0,
            MAX_BASE64_BYTES as f64 / 1_048_576.0,
        );
        let scaled_url = resize_image_to_data_url(image_path, MAX_BASE64_BYTES)?;
        return call_vision_llm(&scaled_url, prompt, model, Some(image_path)).await;
    }

    let result = call_vision_llm(image_data_url, prompt, model, Some(image_path)).await;

    // If API rejects due to size, try scaled-down version
    if let Err(ref e) = result {
        let err_lower = e.to_lowercase();
        if err_lower.contains("too large")
            || err_lower.contains("payload")
            || err_lower.contains("max image")
            || err_lower.contains("exceeds")
        {
            tracing::warn!(
                "Vision API rejected large image — scaling to < {:.1} MB",
                TARGET_BASE64_BYTES as f64 / 1_048_576.0,
            );
            let scaled_url = resize_image_to_data_url(image_path, TARGET_BASE64_BYTES)?;
            return call_vision_llm(&scaled_url, prompt, model, Some(image_path)).await;
        }
    }

    result
}

/// Handle vision_analyze tool call.
pub fn handle_vision_analyze(args: Value) -> Result<String, hermes_core::HermesError> {
    let image_url = args
        .get("image_url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "vision_analyze requires 'image_url' parameter",
            )
        })?
        .to_string();

    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "vision_analyze requires 'question' parameter",
            )
        })?
        .to_string();

    let model = args.get("model").and_then(Value::as_str).map(String::from);

    let prompt = format!(
        "Fully describe and explain everything about this image, then answer the following question:\n\n{question}"
    );

    // Resolve image source: local file path or remote URL
    let temp_image_path: PathBuf;
    let should_cleanup: bool;
    let image_path: PathBuf;

    let expanded = shellexpand::tilde(&image_url);
    let local_path = PathBuf::from(expanded.as_ref());

    if local_path.is_file() {
        // Local file — use directly, don't delete
        image_path = local_path;
        should_cleanup = false;
    } else if validate_image_url(&image_url) {
        // SSRF check before downloading
        if !url_safety::is_safe_url(&image_url) {
            return Ok(tool_error(
                "Blocked: image URL points to a private or internal address (possible SSRF)."
            ));
        }

        // Remote URL — download to temp directory
        temp_image_path = std::env::temp_dir().join(format!(
            "hermes_vision_temp_{}.jpg",
            uuid::Uuid::new_v4()
        ));
        should_cleanup = true;

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return Ok(tool_error("No async runtime available".to_string())),
        };
        if let Err(e) = handle.block_on(download_image(&image_url, &temp_image_path)) {
            return Ok(tool_error(format!("Failed to download image: {e}")));
        }
        image_path = temp_image_path;
    } else {
        return Ok(tool_error(
            "Invalid image source. Provide an HTTP/HTTPS URL or a valid local file path.",
        ));
    }

    // Detect MIME type
    let _mime_type = match detect_mime_type(&image_path) {
        Some(m) => m,
        None => return Ok(tool_error("Only image files are supported for vision analysis.")),
    };

    // Convert to base64 data URL
    let data_url = match image_to_data_url(&image_path) {
        Ok(u) => u,
        Err(e) => return Ok(tool_error(format!("Failed to encode image: {e}"))),
    };

    // Call vision LLM
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return Ok(tool_error("No async runtime available".to_string())),
    };
    let result =
        handle.block_on(call_vision_llm_with_resize(&data_url, &prompt, model.as_deref(), &image_path));

    // Cleanup temp file
    if should_cleanup {
        if let Err(e) = std::fs::remove_file(&image_path) {
            tracing::warn!("Failed to clean up vision temp file: {e}");
        }
    }

    match result {
        Ok(analysis) => Ok(serde_json::json!({
            "success": true,
            "analysis": analysis,
        })
        .to_string()),
        Err(e) => {
            // Categorize error
            let err_lower = e.to_lowercase();
            let analysis = if err_lower.contains("402")
                || err_lower.contains("insufficient")
                || err_lower.contains("payment required")
                || err_lower.contains("credits")
                || err_lower.contains("billing")
            {
                format!("Insufficient credits or payment required. Please top up your API provider account and try again. Error: {e}")
            } else if err_lower.contains("does not support")
                || err_lower.contains("not support image")
                || err_lower.contains("invalid_request")
                || err_lower.contains("content_policy")
                || err_lower.contains("image_url")
                || err_lower.contains("multimodal")
            {
                let model_name = model.as_deref().unwrap_or("configured model");
                format!("{model_name} does not support vision or our request was not accepted by the server. Error: {e}")
            } else {
                format!("There was a problem with the request and the image could not be analyzed. Error: {e}")
            };

            Ok(serde_json::json!({
                "success": false,
                "error": e,
                "analysis": analysis,
            })
            .to_string())
        }
    }
}

/// Register the vision_analyze tool.
pub fn register_vision_tool(registry: &mut ToolRegistry) {
    registry.register(
        "vision_analyze".to_string(),
        "vision".to_string(),
        serde_json::json!({
            "name": "vision_analyze",
            "description": "Analyze images using AI vision. Provides a comprehensive description and answers a specific question about the image content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "image_url": { "type": "string", "description": "Image URL (http/https) or local file path to analyze." },
                    "question": { "type": "string", "description": "Your specific question or request about the image to resolve. The AI will automatically provide a complete image description AND answer your specific question." },
                    "model": { "type": "string", "description": "Vision model to use (optional, uses auxiliary model chain by default)." }
                },
                "required": ["image_url", "question"]
            }
        }),
        std::sync::Arc::new(handle_vision_analyze),
        Some(std::sync::Arc::new(check_vision_requirements)),
        vec![],
        "Analyze images using AI vision models".to_string(),
        "👁️".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_image_url() {
        assert!(validate_image_url("https://example.com/image.jpg"));
        assert!(validate_image_url("http://example.com/photo.png"));
        assert!(!validate_image_url("ftp://example.com/image.jpg"));
        assert!(!validate_image_url(""));
        assert!(!validate_image_url("not a url"));
        assert!(!validate_image_url("example.com/no-protocol"));
    }

    #[test]
    fn test_detect_mime_type_jpeg() {
        let tmp = std::env::temp_dir().join("test_vision_detect.jpg");
        std::fs::write(&tmp, b"\xff\xd8\xff\xe0JFIF").unwrap();
        assert_eq!(detect_mime_type(&tmp), Some("image/jpeg"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_detect_mime_type_png() {
        let tmp = std::env::temp_dir().join("test_vision_detect.png");
        std::fs::write(&tmp, b"\x89PNG\r\n\x1a\n\x00").unwrap();
        assert_eq!(detect_mime_type(&tmp), Some("image/png"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_detect_mime_type_gif() {
        let tmp = std::env::temp_dir().join("test_vision_detect.gif");
        std::fs::write(&tmp, b"GIF89a").unwrap();
        assert_eq!(detect_mime_type(&tmp), Some("image/gif"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_detect_mime_type_webp() {
        let tmp = std::env::temp_dir().join("test_vision_detect.webp");
        std::fs::write(&tmp, b"RIFF\x00\x00\x00\x00WEBP").unwrap();
        assert_eq!(detect_mime_type(&tmp), Some("image/webp"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_detect_mime_type_by_extension() {
        let tmp = std::env::temp_dir().join("test_detect.svg");
        std::fs::write(&tmp, b"<svg></svg>").unwrap();
        assert_eq!(detect_mime_type(&tmp), Some("image/svg+xml"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_detect_mime_type_not_image() {
        let tmp = std::env::temp_dir().join("test_detect.txt");
        std::fs::write(&tmp, b"hello world").unwrap();
        assert!(detect_mime_type(&tmp).is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_handler_missing_image_url() {
        let result = handle_vision_analyze(serde_json::json!({
            "question": "what is this?"
        }));
        // Missing required parameter returns Err from handler
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_missing_question() {
        let result = handle_vision_analyze(serde_json::json!({
            "image_url": "https://example.com/img.jpg"
        }));
        // Missing required parameter returns Err from handler
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_invalid_url() {
        let result = handle_vision_analyze(serde_json::json!({
            "image_url": "not-a-valid-url",
            "question": "what is this?"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_handler_local_file_not_found() {
        let result = handle_vision_analyze(serde_json::json!({
            "image_url": "/nonexistent/path/image.jpg",
            "question": "what is this?"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_check_vision_requirements() {
        // May or may not pass depending on env
        let _ = check_vision_requirements();
    }

    #[test]
    fn test_resize_small_image() {
        // Create a small 16x16 JPEG (RGB, not RGBA)
        let tmp = std::env::temp_dir().join("test_vision_resize.jpg");
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([255, 0, 0]));
        img.save(&tmp).unwrap();

        let result = resize_image_to_data_url(&tmp, MAX_BASE64_BYTES);
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with("data:image/jpeg;base64,"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_resize_large_image_fits_target() {
        // Create a 64x64 image (small enough to fit within target)
        let tmp = std::env::temp_dir().join("test_vision_resize2.jpg");
        let img = image::RgbImage::from_pixel(64, 64, image::Rgb([0, 255, 0]));
        img.save(&tmp).unwrap();

        let result = resize_image_to_data_url(&tmp, TARGET_BASE64_BYTES);
        assert!(result.is_ok());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_max_base64_bytes_constant() {
        assert_eq!(MAX_BASE64_BYTES, 20 * 1024 * 1024);
        assert_eq!(TARGET_BASE64_BYTES, 5 * 1024 * 1024);
    }
}
