#![allow(dead_code)]
//! Clipboard image extraction for macOS, Windows, Linux, and WSL2.
//!
//! Mirrors Python `hermes_cli/clipboard.py`.
//! Uses only OS-level CLI tools — no external crate dependencies.

use std::path::Path;

/// Extract an image from the system clipboard and save it as PNG.
///
/// Returns `true` if an image was found and saved, `false` otherwise.
pub fn save_clipboard_image(dest: &Path) -> bool {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    #[cfg(target_os = "macos")]
    return macos_save(dest);

    #[cfg(target_os = "windows")]
    return windows_save(dest);

    #[cfg(target_os = "linux")]
    return linux_save(dest);

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        tracing::debug!("Clipboard extraction not supported on this platform");
        false
    }
}

/// Quick check: does the clipboard currently contain an image?
///
/// Lighter than `save_clipboard_image` — doesn't extract or write anything.
pub fn has_clipboard_image() -> bool {
    #[cfg(target_os = "macos")]
    return macos_has_image();

    #[cfg(target_os = "windows")]
    return windows_has_image();

    #[cfg(target_os = "linux")]
    return linux_has_image();

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    false
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn macos_save(dest: &Path) -> bool {
    macos_pngpaste(dest) || macos_osascript(dest)
}

#[cfg(target_os = "macos")]
fn macos_has_image() -> bool {
    let output = std::process::Command::new("osascript")
        .args(["-e", "clipboard info"])
        .output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains("class PNGf") || stdout.contains("class TIFF")
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn macos_pngpaste(dest: &Path) -> bool {
    match std::process::Command::new("pngpaste")
        .arg(dest)
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn macos_osascript(dest: &Path) -> bool {
    let script = format!(
        r#"set theFile to POSIX file "{}"
        set theImage to the clipboard as «class PNGf»
        set theFileRef to open for access theFile with write permission
        set eof of theFileRef to 0
        write theImage to theFileRef
        close access theFileRef"#,
        dest.display()
    );
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn windows_save(dest: &Path) -> bool {
    let ps_script = format!(
        r#"Add-Type -AssemblyName System.Windows.Forms;
        $img = [System.Windows.Forms.Clipboard]::GetImage();
        if ($img) {{
            $img.Save('{}');
            exit 0;
        }} else {{
            exit 1;
        }}"#,
        dest.display()
    );
    match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &ps_script])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "windows")]
fn windows_has_image() -> bool {
    let ps_script = r#"Add-Type -AssemblyName System.Windows.Forms;
    if ([System.Windows.Forms.Clipboard]::ContainsImage()) { exit 0 } else { exit 1 }"#;
    match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", ps_script])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn linux_save(dest: &Path) -> bool {
    if is_wsl() {
        return wsl_save(dest);
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return wayland_save(dest);
    }
    xclip_save(dest)
}

#[cfg(target_os = "linux")]
fn linux_has_image() -> bool {
    if is_wsl() {
        return wsl_has_image();
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return wayland_has_image();
    }
    xclip_has_image()
}

#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    // Check for WSL2 by reading /proc/version
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        version.to_lowercase().contains("microsoft") || version.to_lowercase().contains("wsl")
    } else {
        false
    }
}

#[cfg(target_os = "linux")]
fn wayland_save(dest: &Path) -> bool {
    match std::process::Command::new("wl-paste")
        .args(["--type", "image/png"])
        .output()
    {
        Ok(out) => {
            if out.status.success() && !out.stdout.is_empty() {
                std::fs::write(dest, &out.stdout).is_ok()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn wayland_has_image() -> bool {
    match std::process::Command::new("wl-paste")
        .args(["--list-types"])
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains("image/png") || stdout.contains("image/")
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn xclip_save(dest: &Path) -> bool {
    match std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .output()
    {
        Ok(out) => {
            if out.status.success() && !out.stdout.is_empty() {
                std::fs::write(dest, &out.stdout).is_ok()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn xclip_has_image() -> bool {
    match std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains("image/png") || stdout.contains("image/")
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// WSL2
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn wsl_save(dest: &Path) -> bool {
    // Use Windows PowerShell through WSL interop
    let windows_dest = format!(
        r#"{}"#,
        std::process::Command::new("wslpath")
            .arg("-w")
            .arg(dest)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_else(|| dest.to_string_lossy().to_string())
    );

    let ps_script = format!(
        r#"Add-Type -AssemblyName System.Windows.Forms;
        $img = [System.Windows.Forms.Clipboard]::GetImage();
        if ($img) {{
            $img.Save('{}');
            exit 0;
        }} else {{
            exit 1;
        }}"#,
        windows_dest.trim()
    );
    match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &ps_script])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn wsl_has_image() -> bool {
    let ps_script = r#"Add-Type -AssemblyName System.Windows.Forms;
    if ([System.Windows.Forms.Clipboard]::ContainsImage()) { exit 0 } else { exit 1 }"#;
    match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", ps_script])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wsl_logic() {
        // Just verify the function doesn't panic
        #[cfg(target_os = "linux")]
        let _ = is_wsl();
    }

    #[test]
    fn test_has_clipboard_image_no_panic() {
        // Should not panic on any platform
        let _ = has_clipboard_image();
    }
}
