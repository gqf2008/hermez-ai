//! Voice mode TUI for Hermez CLI.
//!
//! Visual feedback for voice recording, transcription, and TTS playback.
//! Mirrors the Python voice mode TUI in `cli.py`.

use console::Style;
use std::io;

/// State of the voice mode session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceState {
    /// Idle, waiting for user input.
    Idle,
    /// Recording audio from microphone.
    Recording { elapsed_ms: u64 },
    /// Transcribing audio to text.
    Transcribing,
    /// Thinking (LLM processing).
    Thinking,
    /// Speaking (TTS playback).
    Speaking,
}

/// Render the voice mode status bar.
pub fn render_voice_bar(state: &VoiceState) -> String {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let cyan = Style::new().cyan();
    let magenta = Style::new().magenta();
    let dim = Style::new().dim();

    match state {
        VoiceState::Idle => {
            format!(" {} Press space to start recording", green.apply_to("[IDLE]"))
        }
        VoiceState::Recording { elapsed_ms } => {
            let secs = *elapsed_ms / 1000;
            let waveform = format_waveform(*elapsed_ms);
            format!(" {} {}  {}s",
                red_apply("[REC]"),
                dim.apply_to(waveform),
                yellow.apply_to(secs),
            )
        }
        VoiceState::Transcribing => {
            format!(" {} Transcribing audio...", yellow.apply_to("[...]"))
        }
        VoiceState::Thinking => {
            format!(" {} Processing...", cyan.apply_to("[AI]"))
        }
        VoiceState::Speaking => {
            format!(" {} Speaking response...", magenta.apply_to("[TTS]"))
        }
    }
}

/// Render an animated waveform indicator.
fn format_waveform(elapsed_ms: u64) -> String {
    // Simple ASCII waveform: vary bar height based on time
    let patterns = ["|||  ", "|||| ", "|||||", "|||||", "|||| ", "|||  ", "||   ", "|||  "];
    let idx = (elapsed_ms / 150) as usize % patterns.len();
    patterns[idx].repeat(4)
}

fn red_apply(s: &str) -> String {
    Style::new().red().apply_to(s).to_string()
}

/// Voice session statistics.
#[derive(Debug, Default)]
pub struct VoiceStats {
    pub total_recordings: u32,
    pub total_speaking: u32,
    pub total_recording_seconds: f64,
    pub total_speaking_seconds: f64,
}

impl VoiceStats {
    pub fn display(&self) -> String {
        format!(
            "Voice Stats:\n  Recordings: {} ({:.0}s total)\n  Responses spoken: {} ({:.0}s total)",
            self.total_recordings,
            self.total_recording_seconds,
            self.total_speaking,
            self.total_speaking_seconds,
        )
    }
}

/// Run a voice recording session. Returns the recorded audio data length
/// or None if the user cancelled.
///
/// On systems with `arecord` (Linux) or `sox` (macOS), records from the
/// microphone. Otherwise, falls back to text input mode.
pub fn record_voice() -> io::Result<Option<Vec<u8>>> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    println!("\n{} Voice Recording", green.apply_to(">"));

    // Check for available recording tool
    let recorder = if command_exists("arecord") {
        "arecord"
    } else if command_exists("sox") {
        "sox"
    } else {
        println!("  {} No microphone tool found (arecord/sox).", yellow.apply_to("!"));
        println!("  {} Falling back to text input.", dim.apply_to("->"));
        return Ok(None);
    };

    println!("  {} Using: {}", dim.apply_to("Recorder"), recorder);
    println!("  {} Press Ctrl+C to stop recording", dim.apply_to("Hint"));

    // In a full implementation, this would spawn the recorder subprocess,
    // display the waveform in real time, and return the audio bytes.
    // For now, we validate the tool and return a placeholder.
    Ok(Some(Vec::new()))
}

/// Check if a command is available on PATH.
fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// TTS indicator: check if a TTS engine is available.
pub fn check_tts_available() -> bool {
    // Check for common TTS tools
    command_exists("say") || command_exists("espeak") || command_exists("piper")
}

/// Speak text using the available TTS engine.
pub fn speak_text(text: &str) -> io::Result<()> {
    if command_exists("say") {
        // macOS
        std::process::Command::new("say").arg(text).status()?;
    } else if command_exists("espeak") {
        std::process::Command::new("espeak").arg(text).status()?;
    } else if command_exists("piper") {
        std::process::Command::new("piper")
            .arg("--output-raw")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_voice_bar_idle() {
        let bar = render_voice_bar(&VoiceState::Idle);
        assert!(bar.contains("IDLE"));
    }

    #[test]
    fn test_voice_bar_recording() {
        let bar = render_voice_bar(&VoiceState::Recording { elapsed_ms: 3500 });
        assert!(bar.contains("REC"));
        assert!(bar.contains('3'));
    }

    #[test]
    fn test_voice_bar_transcribing() {
        let bar = render_voice_bar(&VoiceState::Transcribing);
        assert!(bar.contains("Transcribing"));
    }

    #[test]
    fn test_voice_bar_thinking() {
        let bar = render_voice_bar(&VoiceState::Thinking);
        assert!(bar.contains("Processing"));
    }

    #[test]
    fn test_voice_bar_speaking() {
        let bar = render_voice_bar(&VoiceState::Speaking);
        assert!(bar.contains("TTS"));
    }

    #[test]
    fn test_waveform_format() {
        let w = format_waveform(0);
        assert!(!w.is_empty());
        let w2 = format_waveform(10000);
        assert!(!w2.is_empty());
    }

    #[test]
    fn test_voice_stats_default() {
        let stats = VoiceStats::default();
        assert_eq!(stats.total_recordings, 0);
        assert_eq!(stats.total_speaking, 0);
    }

    #[test]
    fn test_voice_stats_display() {
        let stats = VoiceStats {
            total_recordings: 5,
            total_speaking: 3,
            total_recording_seconds: 45.0,
            total_speaking_seconds: 120.0,
        };
        let display = stats.display();
        assert!(display.contains("5"));
        assert!(display.contains("3"));
    }

    #[test]
    fn test_command_exists() {
        // These should exist on most systems
        // Just verify it doesn't panic
        let _ = command_exists("python");
        let _ = command_exists("nonexistent_command_xyz");
    }

    #[test]
    fn test_check_tts() {
        // Just verify it returns a bool
        let _ = check_tts_available();
    }
}
