#![allow(dead_code)]
//! Text-to-speech tool.
//!
//! Mirrors the Python `tools/tts_tool.py`.
//! 1 tool: `text_to_speech` — converts text to audio via multiple TTS providers.
//! Supports Edge TTS (free), ElevenLabs, OpenAI, MiniMax, and NeuTTS.

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// TTS provider names.
const TTS_PROVIDERS: &[&str] = &["edge", "elevenlabs", "openai", "minimax", "neutts"];

/// Check if TTS requirements are met.
pub fn check_tts_requirements() -> bool {
    // At least Edge TTS (free, no API key) should be available
    true
}

/// Validate text for TTS.
fn validate_text(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("Text cannot be empty.".to_string());
    }

    let max_len = 4000;
    if text.len() > max_len {
        return Err(format!(
            "Text too long ({} chars, max {max_len}). Please shorten your text.",
            text.len()
        ));
    }

    Ok(text.to_string())
}

/// Call Edge TTS (free, no API key needed).
async fn generate_edge_tts(text: &str, output_path: &str, _format: &str) -> Result<String, String> {
    // Edge TTS uses `edge-tts` CLI or Python library.
    // For Rust, we use the edge-tts CLI if available.
    let output = std::process::Command::new("edge-tts")
        .args(["--text", text])
        .args(["--write-media", output_path])
        .arg("--voice")
        .arg("en-US-AriaNeural")
        .output()
        .map_err(|e| {
            format!(
                "Edge TTS not available. Install with: pip install edge-tts. Error: {e}"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Edge TTS failed: {stderr}"));
    }

    Ok(output_path.to_string())
}

/// Call ElevenLabs API.
async fn generate_elevenlabs(
    text: &str,
    output_path: &str,
    voice: &str,
) -> Result<String, String> {
    let api_key = std::env::var("ELEVENLABS_API_KEY")
        .map_err(|_| "ELEVENLABS_API_KEY not set".to_string())?;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "https://api.elevenlabs.io/v1/text-to-speech/{voice}"
        ))
        .header("xi-api-key", &api_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "text": text,
            "model_id": "eleven_multilingual_v2",
            "output_format": "mp3_44100_128",
        }))
        .send()
        .await
        .map_err(|e| format!("ElevenLabs request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("ElevenLabs returned HTTP {status}: {body}"));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    std::fs::write(output_path, &bytes)
        .map_err(|e| format!("Failed to write audio file: {e}"))?;

    Ok(output_path.to_string())
}

/// Call OpenAI TTS API.
async fn generate_openai(
    text: &str,
    output_path: &str,
    voice: &str,
) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("VOICE_TOOLS_OPENAI_KEY"))
        .map_err(|_| "OPENAI_API_KEY not set".to_string())?;

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": text,
            "voice": voice,
            "response_format": "mp3",
        }))
        .send()
        .await
        .map_err(|e| format!("OpenAI TTS request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OpenAI TTS returned HTTP {status}: {body}"));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    std::fs::write(output_path, &bytes)
        .map_err(|e| format!("Failed to write audio file: {e}"))?;

    Ok(output_path.to_string())
}

/// Call MiniMax TTS API.
async fn generate_minimax(
    text: &str,
    output_path: &str,
    voice: &str,
) -> Result<String, String> {
    let api_key = std::env::var("MINIMAX_API_KEY")
        .map_err(|_| "MINIMAX_API_KEY not set".to_string())?;

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.minimax.chat/v1/t2a_v2")
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "text": text,
            "model": "speech-02-turbo",
            "voice_setting": {
                "voice_id": voice,
            },
        }))
        .send()
        .await
        .map_err(|e| format!("MiniMax TTS request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("MiniMax TTS returned HTTP {status}: {body}"));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse MiniMax response: {e}"))?;

    // MiniMax returns audio as base64 or a URL
    if let Some(audio) = json.get("audio").and_then(Value::as_str) {
        // Base64 encoded audio
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, audio)
            .map_err(|e| format!("Failed to decode audio: {e}"))?;
        std::fs::write(output_path, &bytes)
            .map_err(|e| format!("Failed to write audio file: {e}"))?;
    } else if let Some(url) = json.get("audio_url").and_then(Value::as_str) {
        // Download from URL
        let audio_resp = client.get(url).send().await
            .map_err(|e| format!("Failed to download audio: {e}"))?;
        let bytes = audio_resp.bytes().await
            .map_err(|e| format!("Failed to read audio: {e}"))?;
        std::fs::write(output_path, &bytes)
            .map_err(|e| format!("Failed to write audio file: {e}"))?;
    } else {
        return Err(format!("MiniMax response has no audio: {json}"));
    }

    Ok(output_path.to_string())
}

/// Generate TTS using the specified provider.
async fn generate_tts(
    provider: &str,
    text: &str,
    output_path: &str,
    voice: &str,
) -> Result<String, String> {
    match provider {
        "edge" => generate_edge_tts(text, output_path, "mp3").await,
        "elevenlabs" => generate_elevenlabs(text, output_path, voice).await,
        "openai" => generate_openai(text, output_path, voice).await,
        "minimax" => generate_minimax(text, output_path, voice).await,
        "neutts" => Err("NeuTTS requires local CLI installation. Not implemented.".to_string()),
        other => Err(format!("Unknown TTS provider: {other}")),
    }
}

/// Handle text_to_speech tool call.
pub fn handle_text_to_speech(args: Value) -> Result<String, hermes_core::HermesError> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "text_to_speech requires 'text' parameter",
            )
        })?
        .to_string();

    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("edge")
        .to_string();

    let voice = args
        .get("voice")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();

    let output_path = args
        .get("output_path")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            let uuid = uuid::Uuid::new_v4();
            std::env::temp_dir()
                .join(format!("hermes_tts_{uuid}.mp3"))
                .to_string_lossy()
                .to_string()
        });

    // Validate text
    match validate_text(&text) {
        Ok(_) => {}
        Err(e) => return Ok(tool_error(&e)),
    }

    // Validate provider
    if !TTS_PROVIDERS.contains(&provider.as_str()) {
        return Ok(tool_error(format!(
            "Unknown TTS provider: '{provider}'. Valid providers: {:?}",
            TTS_PROVIDERS
        )));
    }

    // Check API keys for paid providers
    match provider.as_str() {
        "elevenlabs" => {
            if std::env::var("ELEVENLABS_API_KEY").is_err() {
                return Ok(tool_error(
                    "ELEVENLABS_API_KEY not set. Set it to use ElevenLabs TTS.",
                ));
            }
        }
        "openai" => {
            if std::env::var("OPENAI_API_KEY").is_err()
                && std::env::var("VOICE_TOOLS_OPENAI_KEY").is_err()
            {
                return Ok(tool_error(
                    "OPENAI_API_KEY not set. Set it to use OpenAI TTS.",
                ));
            }
        }
        "minimax" => {
            if std::env::var("MINIMAX_API_KEY").is_err() {
                return Ok(tool_error(
                    "MINIMAX_API_KEY not set. Set it to use MiniMax TTS.",
                ));
            }
        }
        _ => {}
    }

    // Generate audio
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return Ok(tool_error("No async runtime available")),
    };

    let result = handle.block_on(generate_tts(&provider, &text, &output_path, &voice));

    match result {
        Ok(path) => Ok(serde_json::json!({
            "success": true,
            "provider": provider,
            "voice": voice,
            "output_path": path,
            "media_tag": format!("MEDIA:{path}"),
            "text_length": text.len(),
        })
        .to_string()),
        Err(e) => Ok(tool_error(format!("TTS generation failed: {e}"))),
    }
}

/// Register the text_to_speech tool.
pub fn register_tts_tool(registry: &mut ToolRegistry) {
    registry.register(
        "text_to_speech".to_string(),
        "tts".to_string(),
        serde_json::json!({
            "name": "text_to_speech",
            "description": "Convert text to speech/audio. Supports multiple providers: Edge TTS (free), ElevenLabs (premium), OpenAI TTS, MiniMax.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to convert to speech (max 4000 chars)." },
                    "provider": { "type": "string", "description": "TTS provider: edge (free, default), elevenlabs, openai, minimax." },
                    "voice": { "type": "string", "description": "Voice ID to use (provider-specific, default varies)." },
                    "output_path": { "type": "string", "description": "Output file path (default: temp MP3 file)." }
                },
                "required": ["text"]
            }
        }),
        std::sync::Arc::new(handle_text_to_speech),
        Some(std::sync::Arc::new(check_tts_requirements)),
        vec!["tts".to_string()],
        "Convert text to speech using TTS providers".to_string(),
        "🔊".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_text_empty() {
        assert!(validate_text("").is_err());
        assert!(validate_text("   ").is_err());
    }

    #[test]
    fn test_validate_text_too_long() {
        let long = "a".repeat(5000);
        let result = validate_text(&long);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too long"));
    }

    #[test]
    fn test_validate_text_ok() {
        assert!(validate_text("Hello world").is_ok());
    }

    #[test]
    fn test_check_tts_requirements() {
        // Always available (Edge TTS check)
        assert!(check_tts_requirements());
    }

    #[test]
    fn test_handler_missing_text() {
        let result = handle_text_to_speech(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_unknown_provider() {
        let result = handle_text_to_speech(serde_json::json!({
            "text": "hello",
            "provider": "unknown"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_handler_empty_text() {
        let result = handle_text_to_speech(serde_json::json!({
            "text": "   "
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }
}
