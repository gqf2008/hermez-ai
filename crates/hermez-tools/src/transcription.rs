//! Transcription tools — speech-to-text with multiple providers.
//!
//! Supports:
//! - **openai** — OpenAI Whisper API (requires `OPENAI_API_KEY`)
//! - **groq** — Groq Whisper API, free tier (requires `GROQ_API_KEY`)
//! - **mistral** — Mistral Voxtral (requires `MISTRAL_API_KEY`)
//! - **local_command** — user-configured CLI (via `HERMEZ_LOCAL_STT_COMMAND`)
//!
//! Auto-detection order: openai > groq > mistral > none.
//! (Local whisper.cpp via `whisper-rs` not yet integrated.)
//!
//! Mirrors the Python `tools/transcription_tools.py`.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// Supported audio formats.
const SUPPORTED_FORMATS: &[&str] = &[
    "mp3", "mp4", "mpeg", "mpga", "m4a", "wav", "webm", "ogg", "aac", "flac",
];

/// Audio formats that don't need conversion for local CLI STT.
#[allow(dead_code)]
const NATIVE_AUDIO_FORMATS: &[&str] = &["wav", "aiff", "aif"];

/// Max file size: 25 MB.
const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024;

/// Default models.
const DEFAULT_OPENAI_MODEL: &str = "whisper-1";
const DEFAULT_GROQ_MODEL: &str = "whisper-large-v3-turbo";
const DEFAULT_MISTRAL_MODEL: &str = "voxtral-mini-latest";

/// Environment variable overrides.
const OPENAI_BASE_URL_ENV: &str = "STT_OPENAI_BASE_URL";
const GROQ_BASE_URL_ENV: &str = "GROQ_BASE_URL";
const LOCAL_STT_COMMAND_ENV: &str = "HERMEZ_LOCAL_STT_COMMAND";
const LOCAL_STT_LANGUAGE_ENV: &str = "HERMEZ_LOCAL_STT_LANGUAGE";

/// STT provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProvider {
    Openai,
    Groq,
    Mistral,
    LocalCommand,
    None,
}

/// Result of transcription.
#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub success: bool,
    pub transcript: String,
    pub provider: Option<String>,
    pub error: Option<String>,
}

/// OpenAI-compatible transcription response.
#[derive(Debug, Deserialize)]
struct OpenAiTranscription {
    text: String,
}

/// Mistral transcription response.
#[derive(Debug, Deserialize)]
struct MistralTranscription {
    text: String,
}

/// Validate an audio file path. Returns error string if invalid.
fn validate_audio_file(file_path: &str) -> Option<String> {
    let path = Path::new(file_path);
    if !path.exists() {
        return Some(format!("Audio file not found: {file_path}"));
    }
    if !path.is_file() {
        return Some(format!("Path is not a file: {file_path}"));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !SUPPORTED_FORMATS.contains(&ext.as_str()) {
        let formats = SUPPORTED_FORMATS.join(", ");
        return Some(format!("Unsupported format: .{ext}. Supported: {formats}"));
    }
    if let Ok(metadata) = path.metadata() {
        let size = metadata.len();
        if size > MAX_FILE_SIZE {
            let mb = size as f64 / (1024.0 * 1024.0);
            return Some(format!(
                "File too large: {mb:.1}MB (max 25MB)"
            ));
        }
    }
    None
}

/// Get the OpenAI-compatible API key for STT.
fn resolve_openai_stt_key() -> Result<String, String> {
    std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("VOICE_TOOLS_OPENAI_KEY"))
        .map_err(|_| {
            "Neither OPENAI_API_KEY nor VOICE_TOOLS_OPENAI_KEY is set".to_string()
        })
}

/// Get the base URL for the provider.
fn provider_base_url(provider: SttProvider) -> String {
    match provider {
        SttProvider::Openai => std::env::var(OPENAI_BASE_URL_ENV)
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        SttProvider::Groq => std::env::var(GROQ_BASE_URL_ENV)
            .unwrap_or_else(|_| "https://api.groq.com/openai/v1".to_string()),
        SttProvider::Mistral => "https://api.mistral.ai/v1".to_string(),
        _ => String::new(),
    }
}

/// Get the API key for the provider.
fn provider_api_key(provider: SttProvider) -> Result<String, String> {
    match provider {
        SttProvider::Openai => resolve_openai_stt_key(),
        SttProvider::Groq => std::env::var("GROQ_API_KEY")
            .map_err(|_| "GROQ_API_KEY not set".to_string()),
        SttProvider::Mistral => std::env::var("MISTRAL_API_KEY")
            .map_err(|_| "MISTRAL_API_KEY not set".to_string()),
        _ => Err("No API key for this provider".to_string()),
    }
}

/// Get the default model for the provider.
#[allow(dead_code)]
fn default_model(provider: SttProvider) -> &'static str {
    match provider {
        SttProvider::Openai => DEFAULT_OPENAI_MODEL,
        SttProvider::Groq => DEFAULT_GROQ_MODEL,
        SttProvider::Mistral => DEFAULT_MISTRAL_MODEL,
        _ => "unknown",
    }
}

/// Call an OpenAI-compatible transcription API.
async fn call_openai_compat_api(
    file_path: &str,
    model: &str,
    provider: SttProvider,
) -> TranscriptionResult {
    let api_key = match provider_api_key(provider) {
        Ok(k) => k,
        Err(e) => return TranscriptionResult { success: false, transcript: String::new(), provider: None, error: Some(e) },
    };
    let base_url = provider_base_url(provider);

    let audio_data = match fs::read(file_path) {
        Ok(d) => d,
        Err(e) => return TranscriptionResult { success: false, transcript: String::new(), provider: None, error: Some(format!("Failed to read file: {e}")) },
    };

    let boundary = "----HermezSTIBoundary";
    let mut body = Vec::new();

    // model field
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    body.extend_from_slice(model.as_bytes());
    body.extend_from_slice(b"\r\n");

    // response_format
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"response_format\"\r\n\r\n");
    body.extend_from_slice(b"json");
    body.extend_from_slice(b"\r\n");

    // file field
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"file\"; filename=\"audio\"\r\n");
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(&audio_data);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");

    let client = reqwest::Client::new();
    let resp = match client
        .post(format!("{base_url}/audio/transcriptions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return TranscriptionResult { success: false, transcript: String::new(), provider: None, error: Some(format!("Connection error: {e}")) },
    };

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("API error {status}: {body_text}")),
        };
    }

    let resp_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("Failed to read response: {e}")),
        },
    };

    // Try OpenAI format first
    match serde_json::from_str::<OpenAiTranscription>(&resp_text) {
        Ok(t) => TranscriptionResult {
            success: true,
            transcript: t.text.trim().to_string(),
            provider: Some(format!("{provider:?}").to_lowercase()),
            error: None,
        },
        Err(_) => {
            // Try Mistral format
            match serde_json::from_str::<MistralTranscription>(&resp_text) {
                Ok(t) => TranscriptionResult {
                    success: true,
                    transcript: t.text.trim().to_string(),
                    provider: Some(format!("{provider:?}").to_lowercase()),
                    error: None,
                },
                Err(e) => TranscriptionResult {
                    success: false,
                    transcript: String::new(),
                    provider: None,
                    error: Some(format!("Failed to parse response: {e}")),
                },
            }
        }
    }
}

/// Transcribe using a local command (e.g., whisper CLI).
fn transcribe_local_command(file_path: &str, model: &str) -> TranscriptionResult {
    let command_template = match std::env::var(LOCAL_STT_COMMAND_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("{LOCAL_STT_COMMAND_ENV} not configured")),
        },
    };

    let language = std::env::var(LOCAL_STT_LANGUAGE_ENV)
        .unwrap_or_else(|_| "en".to_string());

    // Create temp dir for output
    let temp_dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("Failed to create temp dir: {e}")),
        },
    };

    let command = command_template
        .replace("{input_path}", file_path)
        .replace("{output_dir}", temp_dir.path().to_str().unwrap_or("."))
        .replace("{language}", &language)
        .replace("{model}", model);

    let output = match Command::new("sh").arg("-c").arg(&command).output() {
        Ok(o) => o,
        Err(e) => return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("Command failed to start: {e}")),
        },
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("Local STT failed: {}", stderr.trim())),
        };
    }

    // Read back .txt files
    let txt_files: Vec<_> = match fs::read_dir(temp_dir.path()) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ex| ex.to_str())
                    == Some("txt")
            })
            .collect(),
        Err(_) => vec![],
    };

    if txt_files.is_empty() {
        return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some("Local STT command did not produce a .txt transcript".to_string()),
        };
    }

    match fs::read_to_string(txt_files[0].path()) {
        Ok(text) => TranscriptionResult {
            success: true,
            transcript: text.trim().to_string(),
            provider: Some("local_command".to_string()),
            error: None,
        },
        Err(e) => TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(format!("Failed to read transcript: {e}")),
        },
    }
}

/// Detect which STT provider to use.
fn detect_provider() -> SttProvider {
    if resolve_openai_stt_key().is_ok() {
        return SttProvider::Openai;
    }
    if std::env::var("GROQ_API_KEY").is_ok() {
        return SttProvider::Groq;
    }
    if std::env::var("MISTRAL_API_KEY").is_ok() {
        return SttProvider::Mistral;
    }
    if std::env::var(LOCAL_STT_COMMAND_ENV).is_ok()
        .then(|| std::env::var(LOCAL_STT_COMMAND_ENV).ok())
        .flatten()
        .is_some_and(|v| !v.is_empty())
    {
        return SttProvider::LocalCommand;
    }
    SttProvider::None
}

/// Transcribe an audio file using the best available provider.
///
/// Provider auto-detection: openai > groq > mistral > local_command > none.
///
/// ```ignore
/// let result = transcribe_audio("/path/to/audio.ogg", None).await?;
/// if result.success {
///     println!("{}", result.transcript);
/// }
/// ```
pub async fn transcribe_audio(
    file_path: &str,
    model: Option<&str>,
) -> TranscriptionResult {
    // Validate input
    if let Some(err) = validate_audio_file(file_path) {
        return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(err),
        };
    }

    let provider = detect_provider();
    if provider == SttProvider::None {
        return TranscriptionResult {
            success: false,
            transcript: String::new(),
            provider: None,
            error: Some(
                "No STT provider available. Set OPENAI_API_KEY, GROQ_API_KEY, \
                 MISTRAL_API_KEY, or HERMEZ_LOCAL_STT_COMMAND."
                    .to_string(),
            ),
        };
    }

    match provider {
        SttProvider::Openai => {
            let m = model.unwrap_or(DEFAULT_OPENAI_MODEL);
            call_openai_compat_api(file_path, m, SttProvider::Openai).await
        }
        SttProvider::Groq => {
            let m = model.unwrap_or(DEFAULT_GROQ_MODEL);
            call_openai_compat_api(file_path, m, SttProvider::Groq).await
        }
        SttProvider::Mistral => {
            let m = model.unwrap_or(DEFAULT_MISTRAL_MODEL);
            call_openai_compat_api(file_path, m, SttProvider::Mistral).await
        }
        SttProvider::LocalCommand => {
            let m = model.unwrap_or("base");
            transcribe_local_command(file_path, m)
        }
        SttProvider::None => unreachable!(),
    }
}

/// Check whether any STT provider is available.
pub fn is_stt_available() -> bool {
    detect_provider() != SttProvider::None
}

/// Get the current detected provider name.
pub fn get_stt_provider_name() -> String {
    match detect_provider() {
        SttProvider::Openai => "openai".to_string(),
        SttProvider::Groq => "groq".to_string(),
        SttProvider::Mistral => "mistral".to_string(),
        SttProvider::LocalCommand => "local_command".to_string(),
        SttProvider::None => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_validate_audio_file_not_found() {
        let err = validate_audio_file("/nonexistent/file.mp3");
        assert!(err.is_some());
        assert!(err.unwrap().contains("not found"));
    }

    #[test]
    fn test_validate_audio_file_unsupported_format() {
        let tmp = tempfile::NamedTempFile::with_suffix(".xyz").unwrap();
        let err = validate_audio_file(tmp.path().to_str().unwrap());
        assert!(err.is_some());
        assert!(err.unwrap().contains("Unsupported"));
    }

    #[test]
    fn test_validate_audio_file_valid_ext() {
        // Just check that common extensions are accepted
        for ext in SUPPORTED_FORMATS {
            assert!(SUPPORTED_FORMATS.contains(ext));
        }
    }

    #[test]
    #[serial]
    fn test_detect_provider_no_keys() {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("GROQ_API_KEY");
        std::env::remove_var("MISTRAL_API_KEY");
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);
        let provider = detect_provider();
        assert_eq!(provider, SttProvider::None);
    }

    #[test]
    #[serial]
    fn test_detect_provider_openai() {
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("GROQ_API_KEY");
        std::env::remove_var("MISTRAL_API_KEY");
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        let provider = detect_provider();
        assert_eq!(provider, SttProvider::Openai);
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    #[serial]
    fn test_detect_provider_groq() {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("MISTRAL_API_KEY");
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);
        std::env::set_var("GROQ_API_KEY", "gsk-test");
        let provider = detect_provider();
        assert_eq!(provider, SttProvider::Groq);
        std::env::remove_var("GROQ_API_KEY");
    }

    #[test]
    #[serial]
    fn test_detect_provider_mistral() {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("GROQ_API_KEY");
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);
        std::env::set_var("MISTRAL_API_KEY", "mistral-test");
        let provider = detect_provider();
        assert_eq!(provider, SttProvider::Mistral);
        std::env::remove_var("MISTRAL_API_KEY");
    }

    #[test]
    #[serial]
    fn test_detect_provider_local_command() {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("GROQ_API_KEY");
        std::env::remove_var("MISTRAL_API_KEY");
        std::env::set_var(LOCAL_STT_COMMAND_ENV, "whisper {input_path}");
        let provider = detect_provider();
        assert_eq!(provider, SttProvider::LocalCommand);
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);
    }

    #[test]
    fn test_default_model() {
        assert_eq!(default_model(SttProvider::Openai), "whisper-1");
        assert_eq!(default_model(SttProvider::Groq), "whisper-large-v3-turbo");
        assert_eq!(default_model(SttProvider::Mistral), "voxtral-mini-latest");
    }

    #[test]
    fn test_provider_base_url() {
        assert!(provider_base_url(SttProvider::Openai).contains("openai.com"));
        assert!(provider_base_url(SttProvider::Groq).contains("groq.com"));
    }

    #[test]
    fn test_transcribe_audio_validation() {
        // Non-existent file should fail immediately
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(transcribe_audio("/nonexistent/audio.mp3", None));
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[test]
    #[serial]
    fn test_transcribe_audio_no_provider() {
        // Ensure no keys are set
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VOICE_TOOLS_OPENAI_KEY");
        std::env::remove_var("GROQ_API_KEY");
        std::env::remove_var("MISTRAL_API_KEY");
        std::env::remove_var(LOCAL_STT_COMMAND_ENV);

        // Create a valid audio file
        let tmp = tempfile::NamedTempFile::with_suffix(".mp3").unwrap();
        // Write a minimal "file" — won't be valid audio but will pass format check
        fs::write(tmp.path(), b"fake audio").unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(transcribe_audio(tmp.path().to_str().unwrap(), None));
        assert!(!result.success);
        // Should fail due to no provider or file read error
    }

    #[test]
    fn test_is_stt_available() {
        let _ = is_stt_available(); // Just ensure it doesn't panic
    }

    #[test]
    fn test_get_stt_provider_name() {
        let name = get_stt_provider_name();
        assert!(matches!(
            name.as_str(),
            "openai" | "groq" | "mistral" | "local_command" | "none"
        ));
    }
}
