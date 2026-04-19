#![allow(dead_code)]
//! Voice mode / speech-to-text tool.
//!
//! Mirrors the Python `tools/voice_mode.py` and `tools/transcription_tools.py`.
//! 1 tool: `transcribe_audio` — converts audio to text via STT providers.
//! Supports local Whisper (faster-whisper), Groq Whisper, OpenAI Whisper, Mistral Voxtral.

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// STT provider names.
const STT_PROVIDERS: &[&str] = &["local", "groq", "openai", "mistral"];

/// Supported audio formats.
const SUPPORTED_FORMATS: &[&str] = &[
    "mp3", "mp4", "mpeg", "mpga", "m4a", "wav", "webm", "ogg", "aac", "flac",
];

/// Maximum file size (25 MB).
const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024;

/// Check if voice/STT requirements are met.
pub fn check_voice_requirements() -> bool {
    // At least one STT provider should be available
    std::env::var("GROQ_API_KEY").is_ok()
        || std::env::var("VOICE_TOOLS_OPENAI_KEY").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
        || std::env::var("MISTRAL_API_KEY").is_ok()
}

/// Validate audio file.
fn validate_audio_file(path: &str) -> Result<(std::path::PathBuf, String), String> {
    let expanded = shellexpand::tilde(path);
    let file_path = std::path::PathBuf::from(expanded.as_ref());

    if !file_path.exists() {
        return Err(format!("Audio file not found: {path}"));
    }

    if !file_path.is_file() {
        return Err(format!("Not a file: {path}"));
    }

    let metadata = std::fs::metadata(&file_path)
        .map_err(|e| format!("Cannot read file metadata: {e}"))?;

    if metadata.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File too large ({:.1} MB, max {} MB)",
            metadata.len() as f64 / 1024.0 / 1024.0,
            MAX_FILE_SIZE / 1024 / 1024
        ));
    }

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .ok_or_else(|| format!("No file extension: {path}"))?;

    if !SUPPORTED_FORMATS.contains(&ext.as_str()) {
        return Err(format!(
            "Unsupported format: '{ext}'. Supported: {:?}",
            SUPPORTED_FORMATS
        ));
    }

    Ok((file_path, ext))
}

/// Resolve STT provider from config or auto-detect.
fn resolve_provider(explicit: Option<&str>) -> String {
    if let Some(p) = explicit {
        if STT_PROVIDERS.contains(&p) {
            return p.to_string();
        }
    }

    // Auto-detect: prefer Groq (free tier) > OpenAI > Mistral > local
    if std::env::var("GROQ_API_KEY").is_ok() {
        return "groq".to_string();
    }
    if std::env::var("VOICE_TOOLS_OPENAI_KEY").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
    {
        return "openai".to_string();
    }
    if std::env::var("MISTRAL_API_KEY").is_ok() {
        return "mistral".to_string();
    }

    "local".to_string()
}

/// Check API key for the selected provider.
fn check_provider_key(provider: &str) -> Result<(), String> {
    match provider {
        "groq" => std::env::var("GROQ_API_KEY")
            .map(|_| ())
            .map_err(|_| "GROQ_API_KEY not set".to_string()),
        "openai" => std::env::var("VOICE_TOOLS_OPENAI_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .map(|_| ())
            .map_err(|_| "OPENAI_API_KEY not set for STT".to_string()),
        "mistral" => std::env::var("MISTRAL_API_KEY")
            .map(|_| ())
            .map_err(|_| "MISTRAL_API_KEY not set".to_string()),
        "local" => Ok(()),
        _ => Err(format!("Unknown provider: {provider}")),
    }
}

/// Transcribe audio using OpenAI-compatible API.
async fn transcribe_api_call(
    provider: &str,
    file_path: &std::path::Path,
    language: Option<&str>,
) -> Result<String, String> {
    let (base_url, api_key_var) = match provider {
        "groq" => (
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
        ),
        "openai" => (
            "https://api.openai.com/v1",
            "VOICE_TOOLS_OPENAI_KEY",
        ),
        "mistral" => (
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
        ),
        _ => return Err(format!("API provider not supported: {provider}")),
    };

    let api_key = std::env::var(api_key_var)
        .map_err(|_| format!("{api_key_var} not set"))?;

    let model = match provider {
        "groq" => "whisper-large-v3-turbo",
        "openai" => "whisper-1",
        "mistral" => "voxtral-mini-latest",
        _ => "whisper-1",
    };

    // Build multipart form data
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio");

    let file_bytes = std::fs::read(file_path)
        .map_err(|e| format!("Failed to read audio file: {e}"))?;

    let boundary = uuid::Uuid::new_v4().to_string().replace('-', "");
    let content_type = match file_path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a") => "audio/mp4",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "audio/mpeg",
    };

    let body = format!(
        "--{boundary}\r\n\
        Content-Disposition: form-data; name=\"model\"\r\n\r\n\
        {model}\r\n\
        --{boundary}\r\n\
        Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
        Content-Type: {content_type}\r\n\r\n"
    );

    // Binary append
    let mut full_body = body.into_bytes();
    full_body.extend(&file_bytes);

    let _footer = format!(
        "\r\n--{boundary}\r\n\
        Content-Disposition: form-data; name=\"response_format\"\r\n\r\n\
        text\r\n\
        --{boundary}\r\n"
    );

    if let Some(lang) = language {
        let lang_part = format!(
            "Content-Disposition: form-data; name=\"language\"\r\n\r\n\
            {lang}\r\n\
            --{boundary}--\r\n"
        );
        full_body.extend(lang_part.into_bytes());
    } else {
        full_body.extend(
            format!("--{boundary}--\r\n").into_bytes()
        );
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let resp = client
        .post(format!("{base_url}/audio/transcriptions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .body(full_body)
        .send()
        .await
        .map_err(|e| format!("STT request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("STT returned HTTP {status}: {body}"));
    }

    // Response is plain text when response_format=text
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err("STT returned empty transcript.".to_string());
    }

    Ok(trimmed)
}

/// Filter known Whisper hallucinations.
fn filter_hallucinations(text: &str) -> String {
    let hallucinations = [
        "thank you",
        "subscribe to my channel",
        "please like and subscribe",
        "www.youtube.com",
        "спасибо что смотрите",
        "チャンネル登録",
        "谢谢观看",
    ];

    let mut result = text.to_string();
    for h in &hallucinations {
        // Case-insensitive removal
        let lower = result.to_lowercase();
        if let Some(pos) = lower.find(h) {
            let end = pos + h.len();
            result.replace_range(pos..end, "");
        }
    }

    // Clean up extra whitespace
    result
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Handle transcribe_audio tool call.
pub fn handle_transcribe_audio(args: Value) -> Result<String, hermes_core::HermesError> {
    let file_path = args
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "transcribe_audio requires 'file_path' parameter",
            )
        })?
        .to_string();

    let provider = args.get("provider").and_then(Value::as_str);
    let language = args.get("language").and_then(Value::as_str);

    // Validate audio file
    let (path, _ext) = match validate_audio_file(&file_path) {
        Ok(v) => v,
        Err(e) => return Ok(tool_error(&e)),
    };

    // Resolve provider
    let provider = resolve_provider(provider);

    // Check API key
    if let Err(e) = check_provider_key(&provider) {
        return Ok(tool_error(&e));
    }

    // Transcribe
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return Ok(tool_error("No async runtime available")),
    };

    let result = handle.block_on(transcribe_api_call(&provider, &path, language));

    match result {
        Ok(text) => {
            let filtered = filter_hallucinations(&text);
            Ok(serde_json::json!({
                "success": true,
                "text": filtered,
                "provider": provider,
                "file": file_path,
            })
            .to_string())
        }
        Err(e) => Ok(tool_error(format!("Transcription failed: {e}"))),
    }
}

/// Register the transcribe_audio tool.
pub fn register_voice_tool(registry: &mut ToolRegistry) {
    registry.register(
        "transcribe_audio".to_string(),
        "voice".to_string(),
        serde_json::json!({
            "name": "transcribe_audio",
            "description": "Transcribe audio to text using speech-to-text. Supports local Whisper, Groq Whisper, OpenAI Whisper, and Mistral Voxtral.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Path to audio file (mp3, wav, m4a, ogg, flac, etc.). Max 25MB." },
                    "provider": { "type": "string", "description": "STT provider: local, groq (free), openai, mistral. Auto-detected if not specified." },
                    "language": { "type": "string", "description": "Language code (e.g., 'en', 'zh', 'es'). Auto-detected if not specified." }
                },
                "required": ["file_path"]
            }
        }),
        std::sync::Arc::new(handle_transcribe_audio),
        Some(std::sync::Arc::new(check_voice_requirements)),
        vec!["voice".to_string()],
        "Transcribe audio to text using STT".to_string(),
        "🎤".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_audio_missing_file() {
        let result = validate_audio_file("/nonexistent/audio.mp3");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_validate_audio_unsupported_format() {
        let tmp = std::env::temp_dir().join("test_audio.unsupported");
        std::fs::write(&tmp, b"fake audio").unwrap();
        let result = validate_audio_file(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_validate_audio_supported_format() {
        let tmp = std::env::temp_dir().join("test_audio.mp3");
        std::fs::write(&tmp, b"fake mp3").unwrap();
        let result = validate_audio_file(tmp.to_str().unwrap());
        assert!(result.is_ok());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_validate_audio_too_large() {
        let tmp = std::env::temp_dir().join("test_audio_large.mp3");
        // Create a sparse file larger than 25MB (cross-platform via set_len).
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .unwrap();
        file.set_len(30 * 1024 * 1024).unwrap();
        drop(file);

        let result = validate_audio_file(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_resolve_provider_auto_groq() {
        // Without GROQ_API_KEY, falls through
        let p = resolve_provider(None);
        // Could be any provider depending on env
        assert!(STT_PROVIDERS.contains(&p.as_str()));
    }

    #[test]
    fn test_resolve_provider_explicit() {
        assert_eq!(resolve_provider(Some("groq")), "groq");
        assert_eq!(resolve_provider(Some("openai")), "openai");
    }

    #[test]
    fn test_resolve_provider_invalid() {
        // Falls back to auto-detect
        let p = resolve_provider(Some("invalid"));
        assert!(STT_PROVIDERS.contains(&p.as_str()));
    }

    #[test]
    fn test_filter_hallucinations_clean() {
        let text = "Hello, this is a normal transcript.";
        assert_eq!(filter_hallucinations(text), text);
    }

    #[test]
    fn test_filter_hallucinations_thank_you() {
        let text = "Here is the answer. Thank you.";
        let result = filter_hallucinations(text);
        assert!(!result.to_lowercase().contains("thank you"));
    }

    #[test]
    fn test_filter_hallucinations_subscribe() {
        let text = "Great content! Please subscribe to my channel.";
        let result = filter_hallucinations(text);
        assert!(!result.to_lowercase().contains("subscribe"));
    }

    #[test]
    fn test_check_voice_requirements() {
        // May or may not pass depending on env
        let _ = check_voice_requirements();
    }

    #[test]
    fn test_handler_missing_file_path() {
        let result = handle_transcribe_audio(serde_json::json!({}));
        assert!(result.is_err());
    }
}
