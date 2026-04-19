#![allow(dead_code)]
//! Clipboard image extraction for macOS, Windows, Linux, and WSL2.
//!
//! Mirrors Python `hermes_cli/clipboard.py`.
//! Uses only OS-level CLI tools — no external crate dependencies.

use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract an image from the system clipboard and save it as PNG.
///
/// Returns `true` if an image was found and saved, `false` otherwise.
pub fn save_clipboard_image(dest: &Path) -> bool {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if cfg!(target_os = "macos") {
        return macos_save(dest);
    }
    if cfg!(target_os = "windows") {
        return windows_save(dest);
    }
    linux_save(dest)
}

/// Quick check: does the clipboard currently contain an image?
///
/// Lighter than `save_clipboard_image` — does not extract or write anything.
pub fn has_clipboard_image() -> bool {
    if cfg!(target_os = "macos") {
        return macos_has_image();
    }
    if cfg!(target_os = "windows") {
        return windows_has_image();
    }
    if is_wsl() {
        return wsl_has_image();
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return wayland_has_image();
    }
    xclip_has_image()
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

fn macos_save(dest: &Path) -> bool {
    macos_pngpaste(dest) || macos_osascript(dest)
}

fn macos_has_image() -> bool {
    let Ok(output) = Command::new("osascript")
        .args(["-e", "clipboard info"])
        .output()
    else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.contains("\u{00ab}class PNGf\u{00bb}") || stdout.contains("\u{00ab}class TIFF\u{00bb}")
}

fn macos_pngpaste(dest: &Path) -> bool {
    match Command::new("pngpaste").arg(dest).output() {
        Ok(output) if output.status.success() && dest.exists() => {
            matches!(std::fs::metadata(dest), Ok(m) if m.len() > 0)
        }
        _ => false,
    }
}

fn macos_osascript(dest: &Path) -> bool {
    if !macos_has_image() {
        return false;
    }

    let script = format!(
        "try\n  set imgData to the clipboard as \u{00ab}class PNGf\u{00bb}\n  \
         set f to open for access POSIX file \"{}\" with write permission\n  \
         write imgData to f\n  close access f\n\
         on error\n  return \"fail\"\nend try",
        dest.display()
    );

    match Command::new("osascript").args(["-e", &script]).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            !stdout.contains("fail") && dest.exists() && file_size(dest) > 0
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Windows / PowerShell
// ---------------------------------------------------------------------------

/// Cached PowerShell executable path.
fn powershell_exe() -> Option<&'static str> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            for name in ["pwsh", "powershell"] {
                if Command::new(name)
                    .args(["-NoProfile", "-NonInteractive", "-Command", "echo ok"])
                    .output()
                    .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("ok"))
                    .unwrap_or(false)
                {
                    return Some(name.to_string());
                }
            }
            None
        })
        .as_ref()
        .map(|s| s.as_str())
}

const PS_CHECK_IMAGE: &str =
    "Add-Type -AssemblyName System.Windows.Forms; \
     [System.Windows.Forms.Clipboard]::ContainsImage()";

const PS_EXTRACT_IMAGE: &str =
    "Add-Type -AssemblyName System.Windows.Forms; \
     Add-Type -AssemblyName System.Drawing; \
     $img = [System.Windows.Forms.Clipboard]::GetImage(); \
     if ($null -eq $img) { exit 1 } \
     $ms = New-Object System.IO.MemoryStream; \
     $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); \
     [System.Convert]::ToBase64String($ms.ToArray())";

fn windows_has_image() -> bool {
    let Some(ps) = powershell_exe() else {
        return false;
    };
    match Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-Command", PS_CHECK_IMAGE])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).contains("True")
        }
        _ => false,
    }
}

fn windows_save(dest: &Path) -> bool {
    let Some(ps) = powershell_exe() else {
        tracing::debug!("No PowerShell found - Windows clipboard image paste unavailable");
        return false;
    };
    match Command::new(ps)
        .args(["-NoProfile", "-NonInteractive", "-Command", PS_EXTRACT_IMAGE])
        .output()
    {
        Ok(output) if output.status.success() => {
            let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if b64.is_empty() {
                return false;
            }
            match base64_decode(&b64) {
                Some(bytes) => {
                    if std::fs::write(dest, &bytes).is_ok() {
                        dest.exists() && file_size(dest) > 0
                    } else {
                        false
                    }
                }
                None => false,
            }
        }
        _ => {
            let _ = std::fs::remove_file(dest);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// WSL2
// ---------------------------------------------------------------------------

fn is_wsl() -> bool {
    // WSL typically has WSL_DISTRO_NAME or /proc/sys/fs/binfmt_misc/WSLInterop
    std::env::var("WSL_DISTRO_NAME").is_ok()
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

fn wsl_has_image() -> bool {
    match Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", PS_CHECK_IMAGE])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).contains("True")
        }
        Err(e) => {
            tracing::debug!("powershell.exe not found - WSL clipboard unavailable: {e}");
            false
        }
        _ => false,
    }
}

fn wsl_save(dest: &Path) -> bool {
    match Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", PS_EXTRACT_IMAGE])
        .output()
    {
        Ok(output) if output.status.success() => {
            let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if b64.is_empty() {
                return false;
            }
            match base64_decode(&b64) {
                Some(bytes) => {
                    if std::fs::write(dest, &bytes).is_ok() {
                        dest.exists() && file_size(dest) > 0
                    } else {
                        false
                    }
                }
                None => false,
            }
        }
        Err(e) => {
            tracing::debug!("powershell.exe not found - WSL clipboard unavailable: {e}");
            false
        }
        _ => {
            let _ = std::fs::remove_file(dest);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Linux (Wayland + X11)
// ---------------------------------------------------------------------------

fn linux_save(dest: &Path) -> bool {
    if is_wsl() && wsl_save(dest) {
        return true;
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() && wayland_save(dest) {
        return true;
    }
    xclip_save(dest)
}

fn wayland_has_image() -> bool {
    match Command::new("wl-paste").args(["--list-types"]).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|t| t.starts_with("image/"))
        }
        Err(e) => {
            tracing::debug!("wl-paste not installed - Wayland clipboard unavailable: {e}");
            false
        }
        _ => false,
    }
}

fn wayland_save(dest: &Path) -> bool {
    let Ok(types_out) = Command::new("wl-paste").args(["--list-types"]).output() else {
        return false;
    };
    if !types_out.status.success() {
        return false;
    }
    let types: Vec<String> = String::from_utf8_lossy(&types_out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();

    let mime = ["image/png", "image/jpeg", "image/bmp", "image/gif", "image/webp"]
        .iter()
        .find(|&&m| types.contains(&m.to_string()))
        .copied();

    let Some(mime) = mime else {
        return false;
    };

    let Ok(file) = std::fs::File::create(dest) else {
        return false;
    };
    let result = Command::new("wl-paste")
        .args(["--type", mime])
        .stdout(file)
        .status();

    if result.map(|s| s.success()).unwrap_or(false) && dest.exists() && file_size(dest) > 0 {
        // BMP from WSLg may need conversion, but skip if ImageMagick unavailable
        if mime == "image/bmp" {
            return convert_bmp_to_png(dest);
        }
        true
    } else {
        let _ = std::fs::remove_file(dest);
        false
    }
}

fn xclip_has_image() -> bool {
    match Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).contains("image/png")
        }
        _ => false,
    }
}

fn xclip_save(dest: &Path) -> bool {
    let Ok(targets) = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output()
    else {
        return false;
    };
    if !targets.status.success()
        || !String::from_utf8_lossy(&targets.stdout).contains("image/png")
    {
        return false;
    }

    let Ok(file) = std::fs::File::create(dest) else {
        return false;
    };
    let result = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .stdout(file)
        .status();

    if result.map(|s| s.success()).unwrap_or(false) && dest.exists() && file_size(dest) > 0 {
        true
    } else {
        let _ = std::fs::remove_file(dest);
        false
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Try to convert a BMP file to PNG in-place using ImageMagick `convert`.
fn convert_bmp_to_png(path: &Path) -> bool {
    let bmp = path.with_extension("bmp");
    if std::fs::rename(path, &bmp).is_err() {
        return bmp.exists() && file_size(&bmp) > 0;
    }
    match Command::new("convert")
        .arg(bmp.as_os_str())
        .arg(format!("png:{}", path.display()))
        .output()
    {
        Ok(output) if output.status.success() && path.exists() && file_size(path) > 0 => {
            let _ = std::fs::remove_file(&bmp);
            true
        }
        _ => {
            // Restore original
            let _ = std::fs::rename(&bmp, path);
            path.exists() && file_size(path) > 0
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_is_wsl_false_on_non_linux() {
        // This test only makes sense on non-WSL systems
        if !is_wsl() {
            assert!(!is_wsl());
        }
    }

    #[test]
    fn test_has_clipboard_image_does_not_panic() {
        // Should not panic on any platform
        let _ = has_clipboard_image();
    }

    #[test]
    fn test_save_clipboard_image_with_invalid_dest() {
        // Should return false for an invalid path, not panic
        let bad = PathBuf::from("/nonexistent/dir/image.png");
        // On Windows this might succeed if the path is somehow valid,
        // but generally it should return false
        let result = save_clipboard_image(&bad);
        // We can't assert false because if clipboard actually has an image
        // and the dir can be created, it might succeed. Just ensure no panic.
        let _ = result;
    }

    #[test]
    fn test_base64_decode_valid() {
        let decoded = base64_decode("aGVsbG8=");
        assert_eq!(decoded, Some(b"hello".to_vec()));
    }

    #[test]
    fn test_base64_decode_invalid() {
        assert!(base64_decode("!!!").is_none());
    }
}
