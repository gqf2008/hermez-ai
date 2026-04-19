#![allow(dead_code)]
//! Binary file extensions detection.
//!
//! Mirrors the Python `tools/binary_extensions.py`.
//! Pure data — no I/O, no external dependencies.

use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::path::Path;

/// Frozenset of binary file extensions (ported from the Python codebase).
static BINARY_EXTENSIONS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        // Images
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "tif", "webp",
        "svg", "psd", "ai", "eps", "raw", "cr2", "nef", "heic", "avif",
        // Videos
        "mp4", "avi", "mov", "mkv", "wmv", "flv", "webm", "m4v", "mpeg",
        "mpg", "3gp", "ts", "vob", "ogv",
        // Audio
        "mp3", "wav", "flac", "aac", "ogg", "wma", "m4a", "opus", "mid",
        "midi", "aiff", "ape",
        // Archives
        "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "lz", "lzma", "zst",
        "tgz", "tbz2", "cab", "iso", "cpio", "arj",
        // Executables
        "exe", "dll", "so", "dylib", "o", "obj", "a", "lib", "com", "bat",
        "cmd", "msi", "scr", "pif", "app", "dmg",
        // Documents
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods",
        "odp", "epub", "mobi", "azw", "azw3", "djvu",
        // Fonts
        "ttf", "otf", "woff", "woff2", "eot",
        // Bytecode / VM
        "pyc", "pyo", "pyd", "class", "elc", "beam", "wasm",
        // Database
        "db", "sqlite", "sqlite3", "mdb", "accdb",
        // Design / 3D
        "blend", "max", "mb", "fbx", "obj", "stl", "gltf", "glb",
        // Other
        "swf", "fla", "dat", "bin", "rom", "img", "vdi", "qcow", "qcow2",
        "log", "prof", "lock",
    ])
});

/// Check if a file path has a binary extension.
pub fn has_binary_extension(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| BINARY_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Get the set of all binary extensions.
pub fn binary_extensions() -> &'static HashSet<&'static str> {
    &BINARY_EXTENSIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_extension() {
        assert!(has_binary_extension("photo.jpg"));
        assert!(has_binary_extension("image.PNG"));
    }

    #[test]
    fn test_text_extension() {
        assert!(!has_binary_extension("main.py"));
        assert!(!has_binary_extension("Cargo.toml"));
        assert!(!has_binary_extension("README.md"));
    }

    #[test]
    fn test_no_extension() {
        assert!(!has_binary_extension("Makefile"));
        assert!(!has_binary_extension("LICENSE"));
    }

    #[test]
    fn test_hidden_file() {
        assert!(!has_binary_extension(".gitignore"));
        assert!(!has_binary_extension(".env"));
    }
}
