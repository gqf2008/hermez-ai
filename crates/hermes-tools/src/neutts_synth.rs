#![allow(dead_code)]
//! NeuTTS synthesis helper.
//!
//! Spawns `neutts_synth.py` as a subprocess to keep the TTS model (~500MB)
//! in a separate process that exits after synthesis — no lingering memory.

use std::path::PathBuf;
use std::process::Command;

/// Synthesis configuration.
#[derive(Debug, Clone)]
pub struct NeuTtsConfig {
    /// Path to the neutts_synth.py script.
    pub script_path: PathBuf,
    /// Python interpreter path (defaults to "python3").
    pub python_path: String,
    /// HuggingFace backbone model repo.
    pub model: String,
    /// Device (cpu/cuda/mps).
    pub device: String,
}

impl Default for NeuTtsConfig {
    fn default() -> Self {
        Self {
            script_path: PathBuf::from("tools/neutts_synth.py"),
            python_path: "python3".to_string(),
            model: "neuphonic/neutts-air-q4-gguf".to_string(),
            device: "cpu".to_string(),
        }
    }
}

/// Synthesis request.
#[derive(Debug, Clone)]
pub struct NeuTtsRequest {
    /// Text to synthesize.
    pub text: String,
    /// Output WAV path.
    pub output: PathBuf,
    /// Reference voice audio path.
    pub ref_audio: PathBuf,
    /// Reference voice transcript path.
    pub ref_text: PathBuf,
}

/// Result of a synthesis operation.
#[derive(Debug)]
pub struct NeuTtsResult {
    /// Whether synthesis succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
    /// Path to the output WAV file.
    pub output_path: Option<PathBuf>,
}

impl NeuTtsConfig {
    /// Run synthesis synchronously.
    pub fn synthesize(&self, req: &NeuTtsRequest) -> NeuTtsResult {
        // Validate inputs
        if !req.ref_audio.exists() {
            return NeuTtsResult {
                success: false,
                error: Some(format!("Reference audio not found: {}", req.ref_audio.display())),
                output_path: None,
            };
        }
        if !req.ref_text.exists() {
            return NeuTtsResult {
                success: false,
                error: Some(format!("Reference text not found: {}", req.ref_text.display())),
                output_path: None,
            };
        }

        // Ensure output directory exists
        if let Some(parent) = req.output.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let output = Command::new(&self.python_path)
            .arg(&self.script_path)
            .arg("--text")
            .arg(&req.text)
            .arg("--out")
            .arg(&req.output)
            .arg("--ref-audio")
            .arg(&req.ref_audio)
            .arg("--ref-text")
            .arg(&req.ref_text)
            .arg("--model")
            .arg(&self.model)
            .arg("--device")
            .arg(&self.device)
            .output();

        match output {
            Ok(out) => {
                if out.status.success() {
                    NeuTtsResult {
                        success: true,
                        error: None,
                        output_path: Some(req.output.clone()),
                    }
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    NeuTtsResult {
                        success: false,
                        error: Some(format!("Synthesis failed: {stderr}")),
                        output_path: None,
                    }
                }
            }
            Err(e) => NeuTtsResult {
                success: false,
                error: Some(format!("Failed to run neutts_synth: {e}")),
                output_path: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let cfg = NeuTtsConfig::default();
        assert_eq!(cfg.python_path, "python3");
        assert!(cfg.model.contains("neutts"));
    }

    #[test]
    fn test_validate_missing_ref() {
        let cfg = NeuTtsConfig::default();
        let req = NeuTtsRequest {
            text: "hello".to_string(),
            output: PathBuf::from("/tmp/out.wav"),
            ref_audio: PathBuf::from("/nonexistent/ref.wav"),
            ref_text: PathBuf::from("/nonexistent/ref.txt"),
        };
        let result = cfg.synthesize(&req);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }
}
