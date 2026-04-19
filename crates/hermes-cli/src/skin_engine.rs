#![allow(dead_code)]
//! Hermes CLI skin/theme engine.
//!
//! Mirrors Python `hermes_cli/skin_engine.py`.
//! Data-driven skin system that lets users customize the CLI's visual appearance.
//! Skins are defined as YAML files in ~/.hermes/skins/ or as built-in presets.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Complete skin configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkinConfig {
    /// Unique skin name.
    pub name: String,
    /// Short description shown in listings.
    pub description: String,
    /// Color hex values.
    pub colors: HashMap<String, String>,
    /// Spinner customization.
    pub spinner: SpinnerConfig,
    /// Branding text strings.
    pub branding: HashMap<String, String>,
    /// Character for tool output lines.
    pub tool_prefix: String,
    /// Per-tool emoji overrides.
    pub tool_emojis: HashMap<String, String>,
    /// ASCII art logo (Rich markup).
    pub banner_logo: String,
    /// Hero art (Rich markup).
    pub banner_hero: String,
}

/// Spinner customization within a skin.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SpinnerConfig {
    /// Faces shown while waiting for API.
    pub waiting_faces: Vec<String>,
    /// Faces shown during reasoning.
    pub thinking_faces: Vec<String>,
    /// Verbs for spinner messages.
    pub thinking_verbs: Vec<String>,
    /// Wing decorations [left, right] pairs.
    pub wings: Vec<[String; 2]>,
}

impl SkinConfig {
    /// Get a color value with fallback.
    pub fn get_color(&self, key: &str, fallback: &str) -> String {
        self.colors
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Get spinner wing pairs, or empty list if none.
    pub fn get_spinner_wings(&self) -> Vec<(String, String)> {
        self.spinner
            .wings
            .iter()
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect()
    }

    /// Get a branding value with fallback.
    pub fn get_branding(&self, key: &str, fallback: &str) -> String {
        self.branding
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }
}

// ---------------------------------------------------------------------------
// Built-in skins
// ---------------------------------------------------------------------------

fn builtin_default() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#CD7F32".to_string());
    colors.insert("banner_title".to_string(), "#FFD700".to_string());
    colors.insert("banner_accent".to_string(), "#FFBF00".to_string());
    colors.insert("banner_dim".to_string(), "#B8860B".to_string());
    colors.insert("banner_text".to_string(), "#FFF8DC".to_string());
    colors.insert("ui_accent".to_string(), "#FFBF00".to_string());
    colors.insert("ui_label".to_string(), "#4dd0e1".to_string());
    colors.insert("ui_ok".to_string(), "#4caf50".to_string());
    colors.insert("ui_error".to_string(), "#ef5350".to_string());
    colors.insert("ui_warn".to_string(), "#ffa726".to_string());
    colors.insert("prompt".to_string(), "#FFF8DC".to_string());
    colors.insert("input_rule".to_string(), "#CD7F32".to_string());
    colors.insert("response_border".to_string(), "#FFD700".to_string());
    colors.insert("session_label".to_string(), "#DAA520".to_string());
    colors.insert("session_border".to_string(), "#8B8682".to_string());

    let mut branding = HashMap::new();
    branding.insert(
        "agent_name".to_string(),
        "Hermes Agent".to_string(),
    );
    branding.insert(
        "welcome".to_string(),
        "Welcome to Hermes Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Goodbye! ⚕".to_string());
    branding.insert("response_label".to_string(), " ⚕ Hermes ".to_string());
    branding.insert("prompt_symbol".to_string(), "❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(^_^)? Available Commands".to_string(),
    );

    SkinConfig {
        name: "default".to_string(),
        description: "Classic Hermes — gold and kawaii".to_string(),
        colors,
        branding,
        tool_prefix: "┊".to_string(),
        ..Default::default()
    }
}

fn builtin_ares() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#9F1C1C".to_string());
    colors.insert("banner_title".to_string(), "#C7A96B".to_string());
    colors.insert("banner_accent".to_string(), "#DD4A3A".to_string());
    colors.insert("banner_dim".to_string(), "#6B1717".to_string());
    colors.insert("banner_text".to_string(), "#F1E6CF".to_string());
    colors.insert("ui_accent".to_string(), "#DD4A3A".to_string());
    colors.insert("ui_label".to_string(), "#C7A96B".to_string());
    colors.insert("ui_ok".to_string(), "#4caf50".to_string());
    colors.insert("ui_error".to_string(), "#ef5350".to_string());
    colors.insert("ui_warn".to_string(), "#ffa726".to_string());
    colors.insert("prompt".to_string(), "#F1E6CF".to_string());
    colors.insert("input_rule".to_string(), "#9F1C1C".to_string());
    colors.insert("response_border".to_string(), "#C7A96B".to_string());
    colors.insert("session_label".to_string(), "#C7A96B".to_string());
    colors.insert("session_border".to_string(), "#6E584B".to_string());

    let mut branding = HashMap::new();
    branding.insert("agent_name".to_string(), "Ares Agent".to_string());
    branding.insert(
        "welcome".to_string(),
        "Welcome to Ares Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Farewell, warrior! ⚔".to_string());
    branding.insert("response_label".to_string(), " ⚔ Ares ".to_string());
    branding.insert("prompt_symbol".to_string(), "⚔ ❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(⚔) Available Commands".to_string(),
    );

    let spinner = SpinnerConfig {
        waiting_faces: vec![
            "(⚔)".to_string(),
            "(⛨)".to_string(),
            "(▲)".to_string(),
            "(<>)".to_string(),
            "(/)".to_string(),
        ],
        thinking_faces: vec![
            "(⚔)".to_string(),
            "(⛨)".to_string(),
            "(▲)".to_string(),
            "(⌁)".to_string(),
            "(<>)".to_string(),
        ],
        thinking_verbs: vec![
            "forging".to_string(),
            "marching".to_string(),
            "sizing the field".to_string(),
            "holding the line".to_string(),
            "hammering plans".to_string(),
            "tempering steel".to_string(),
            "plotting impact".to_string(),
            "raising the shield".to_string(),
        ],
        wings: vec![
            ["⟪⚔".to_string(), "⚔⟫".to_string()],
            ["⟪▲".to_string(), "▲⟫".to_string()],
            ["⟪╸".to_string(), "╺⟫".to_string()],
            ["⟪⛨".to_string(), "⛨⟫".to_string()],
        ],
    };

    SkinConfig {
        name: "ares".to_string(),
        description: "War-god theme — crimson and bronze".to_string(),
        colors,
        spinner,
        branding,
        tool_prefix: "╎".to_string(),
        ..Default::default()
    }
}

fn builtin_mono() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#555555".to_string());
    colors.insert("banner_title".to_string(), "#e6edf3".to_string());
    colors.insert("banner_accent".to_string(), "#aaaaaa".to_string());
    colors.insert("banner_dim".to_string(), "#444444".to_string());
    colors.insert("banner_text".to_string(), "#c9d1d9".to_string());
    colors.insert("ui_accent".to_string(), "#aaaaaa".to_string());
    colors.insert("ui_label".to_string(), "#888888".to_string());
    colors.insert("ui_ok".to_string(), "#888888".to_string());
    colors.insert("ui_error".to_string(), "#cccccc".to_string());
    colors.insert("ui_warn".to_string(), "#999999".to_string());
    colors.insert("prompt".to_string(), "#c9d1d9".to_string());
    colors.insert("input_rule".to_string(), "#444444".to_string());
    colors.insert("response_border".to_string(), "#aaaaaa".to_string());
    colors.insert("session_label".to_string(), "#888888".to_string());
    colors.insert("session_border".to_string(), "#555555".to_string());

    let mut branding = HashMap::new();
    branding.insert(
        "agent_name".to_string(),
        "Hermes Agent".to_string(),
    );
    branding.insert(
        "welcome".to_string(),
        "Welcome to Hermes Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Goodbye! ⚕".to_string());
    branding.insert("response_label".to_string(), " ⚕ Hermes ".to_string());
    branding.insert("prompt_symbol".to_string(), "❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(^_^)? Available Commands".to_string(),
    );

    SkinConfig {
        name: "mono".to_string(),
        description: "Monochrome — clean grayscale".to_string(),
        colors,
        branding,
        tool_prefix: "┊".to_string(),
        ..Default::default()
    }
}

fn builtin_slate() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#475569".to_string());
    colors.insert("banner_title".to_string(), "#94a3b8".to_string());
    colors.insert("banner_accent".to_string(), "#64748b".to_string());
    colors.insert("banner_dim".to_string(), "#334155".to_string());
    colors.insert("banner_text".to_string(), "#cbd5e1".to_string());
    colors.insert("ui_accent".to_string(), "#60a5fa".to_string());
    colors.insert("ui_label".to_string(), "#38bdf8".to_string());
    colors.insert("ui_ok".to_string(), "#4ade80".to_string());
    colors.insert("ui_error".to_string(), "#f87171".to_string());
    colors.insert("ui_warn".to_string(), "#fbbf24".to_string());
    colors.insert("prompt".to_string(), "#e2e8f0".to_string());
    colors.insert("input_rule".to_string(), "#475569".to_string());
    colors.insert("response_border".to_string(), "#94a3b8".to_string());
    colors.insert("session_label".to_string(), "#64748b".to_string());
    colors.insert("session_border".to_string(), "#475569".to_string());

    let mut branding = HashMap::new();
    branding.insert(
        "agent_name".to_string(),
        "Hermes Agent".to_string(),
    );
    branding.insert(
        "welcome".to_string(),
        "Welcome to Hermes Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Goodbye! ⚕".to_string());
    branding.insert("response_label".to_string(), " ⚕ Hermes ".to_string());
    branding.insert("prompt_symbol".to_string(), "❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(^_^)? Available Commands".to_string(),
    );

    SkinConfig {
        name: "slate".to_string(),
        description: "Cool blue developer-focused theme".to_string(),
        colors,
        branding,
        tool_prefix: "┊".to_string(),
        ..Default::default()
    }
}

fn builtin_daylight() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#94a3b8".to_string());
    colors.insert("banner_title".to_string(), "#1e293b".to_string());
    colors.insert("banner_accent".to_string(), "#475569".to_string());
    colors.insert("banner_dim".to_string(), "#64748b".to_string());
    colors.insert("banner_text".to_string(), "#334155".to_string());
    colors.insert("ui_accent".to_string(), "#2563eb".to_string());
    colors.insert("ui_label".to_string(), "#0284c7".to_string());
    colors.insert("ui_ok".to_string(), "#16a34a".to_string());
    colors.insert("ui_error".to_string(), "#dc2626".to_string());
    colors.insert("ui_warn".to_string(), "#ea580c".to_string());
    colors.insert("prompt".to_string(), "#1e293b".to_string());
    colors.insert("input_rule".to_string(), "#94a3b8".to_string());
    colors.insert("response_border".to_string(), "#475569".to_string());
    colors.insert("session_label".to_string(), "#475569".to_string());
    colors.insert("session_border".to_string(), "#94a3b8".to_string());

    let mut branding = HashMap::new();
    branding.insert(
        "agent_name".to_string(),
        "Hermes Agent".to_string(),
    );
    branding.insert(
        "welcome".to_string(),
        "Welcome to Hermes Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Goodbye! ⚕".to_string());
    branding.insert("response_label".to_string(), " ⚕ Hermes ".to_string());
    branding.insert("prompt_symbol".to_string(), "❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(^_^)? Available Commands".to_string(),
    );

    SkinConfig {
        name: "daylight".to_string(),
        description: "Light background theme with dark text and blue accents".to_string(),
        colors,
        branding,
        tool_prefix: "┊".to_string(),
        ..Default::default()
    }
}

fn builtin_warm_lightmode() -> SkinConfig {
    let mut colors = HashMap::new();
    colors.insert("banner_border".to_string(), "#a1887f".to_string());
    colors.insert("banner_title".to_string(), "#5d4037".to_string());
    colors.insert("banner_accent".to_string(), "#8d6e63".to_string());
    colors.insert("banner_dim".to_string(), "#795548".to_string());
    colors.insert("banner_text".to_string(), "#4e342e".to_string());
    colors.insert("ui_accent".to_string(), "#ff8f00".to_string());
    colors.insert("ui_label".to_string(), "#6d4c41".to_string());
    colors.insert("ui_ok".to_string(), "#2e7d32".to_string());
    colors.insert("ui_error".to_string(), "#c62828".to_string());
    colors.insert("ui_warn".to_string(), "#ef6c00".to_string());
    colors.insert("prompt".to_string(), "#3e2723".to_string());
    colors.insert("input_rule".to_string(), "#a1887f".to_string());
    colors.insert("response_border".to_string(), "#8d6e63".to_string());
    colors.insert("session_label".to_string(), "#6d4c41".to_string());
    colors.insert("session_border".to_string(), "#a1887f".to_string());

    let mut branding = HashMap::new();
    branding.insert(
        "agent_name".to_string(),
        "Hermes Agent".to_string(),
    );
    branding.insert(
        "welcome".to_string(),
        "Welcome to Hermes Agent! Type your message or /help for commands.".to_string(),
    );
    branding.insert("goodbye".to_string(), "Goodbye! ⚕".to_string());
    branding.insert("response_label".to_string(), " ⚕ Hermes ".to_string());
    branding.insert("prompt_symbol".to_string(), "❯ ".to_string());
    branding.insert(
        "help_header".to_string(),
        "(^_^)? Available Commands".to_string(),
    );

    SkinConfig {
        name: "warm-lightmode".to_string(),
        description: "Warm brown/gold text for light terminal backgrounds".to_string(),
        colors,
        branding,
        tool_prefix: "┊".to_string(),
        ..Default::default()
    }
}

/// Return the built-in skin by name, or None if not found.
fn get_builtin_skin(name: &str) -> Option<SkinConfig> {
    match name {
        "default" => Some(builtin_default()),
        "ares" => Some(builtin_ares()),
        "mono" => Some(builtin_mono()),
        "slate" => Some(builtin_slate()),
        "daylight" => Some(builtin_daylight()),
        "warm-lightmode" => Some(builtin_warm_lightmode()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Skin directory and persistence
// ---------------------------------------------------------------------------

fn skins_dir() -> PathBuf {
    let hermes_home = hermes_core::get_hermes_home();
    hermes_home.join("skins")
}

fn skin_file_path(name: &str) -> PathBuf {
    skins_dir().join(format!("{}.yaml", name))
}

/// Load a user-defined skin from ~/.hermes/skins/<name>.yaml.
fn load_user_skin(name: &str) -> Option<SkinConfig> {
    let path = skin_file_path(name);
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut skin: SkinConfig = serde_yaml::from_str(&text).ok()?;
    skin.name = name.to_string();
    Some(skin)
}

/// Save a user-defined skin to ~/.hermes/skins/<name>.yaml.
pub fn save_user_skin(name: &str, skin: &SkinConfig) -> std::io::Result<()> {
    let dir = skins_dir();
    std::fs::create_dir_all(&dir)?;
    let path = skin_file_path(name);
    let text = serde_yaml::to_string(skin).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    std::fs::write(&path, text)
}

// ---------------------------------------------------------------------------
// Active skin persistence
// ---------------------------------------------------------------------------

fn active_skin_path() -> PathBuf {
    let hermes_home = hermes_core::get_hermes_home();
    hermes_home.join(".active_skin")
}

/// Get the currently active skin.
///
/// Priority:
/// 1. ~/.hermes/.active_skin file content
/// 2. Built-in "default" skin
pub fn get_active_skin() -> SkinConfig {
    let path = active_skin_path();
    let name = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());

    resolve_skin(&name).unwrap_or_else(builtin_default)
}

/// Set the active skin by name.
pub fn set_active_skin(name: &str) -> std::io::Result<()> {
    let path = active_skin_path();
    std::fs::write(&path, name)
}

/// Resolve a skin by name: user skins override built-ins.
pub fn resolve_skin(name: &str) -> Option<SkinConfig> {
    load_user_skin(name).or_else(|| get_builtin_skin(name))
}

/// List all available skins (built-in + user-defined).
pub fn list_skins() -> Vec<SkinConfig> {
    let mut skins = vec![
        builtin_default(),
        builtin_ares(),
        builtin_mono(),
        builtin_slate(),
        builtin_daylight(),
        builtin_warm_lightmode(),
    ];

    // Scan user skins directory
    let dir = skins_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if get_builtin_skin(stem).is_none() {
                        if let Some(skin) = load_user_skin(stem) {
                            skins.push(skin);
                        }
                    }
                }
            }
        }
    }

    skins
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a hex color string to a `console::Style` foreground color.
///
/// Supports 6-char hex (`#RRGGBB`) and 3-char hex (`#RGB`).
/// Falls back to ANSI 256 color approximation since `console` does not expose
/// 24-bit RGB directly on Windows.
pub fn hex_to_style(hex: &str) -> Option<console::Style> {
    let trimmed = hex.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().skip(1).collect();

    let (r, g, b) = match chars.len() {
        3 => {
            let r = u8::from_str_radix(&format!("{}{}", chars[0], chars[0]), 16).ok()?;
            let g = u8::from_str_radix(&format!("{}{}", chars[1], chars[1]), 16).ok()?;
            let b = u8::from_str_radix(&format!("{}{}", chars[2], chars[2]), 16).ok()?;
            (r, g, b)
        }
        6 => {
            let r = u8::from_str_radix(&chars[0..2].iter().collect::<String>(), 16).ok()?;
            let g = u8::from_str_radix(&chars[2..4].iter().collect::<String>(), 16).ok()?;
            let b = u8::from_str_radix(&chars[4..6].iter().collect::<String>(), 16).ok()?;
            (r, g, b)
        }
        _ => return None,
    };

    let ansi256 = rgb_to_ansi256(r, g, b);
    Some(console::Style::new().fg(console::Color::Color256(ansi256)))
}

/// Convert RGB to ANSI 256 color code.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    // Grayscale: r == g == b
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (((r as u16 - 8) * 24) / 247) as u8 + 232;
    }

    // 6x6x6 color cube
    let r = ((r as u16 * 5) / 255) as u8;
    let g = ((g as u16 * 5) / 255) as u8;
    let b = ((b as u16 * 5) / 255) as u8;
    16 + 36 * r + 6 * g + b
}

// ---------------------------------------------------------------------------
// CLI commands
// ---------------------------------------------------------------------------

/// CLI: list all available skins (built-in + user-defined).
pub fn cmd_skin_list() -> anyhow::Result<()> {
    use console::Style;

    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let dim = Style::new().dim();

    let active = get_active_skin();
    let skins = list_skins();

    println!();
    println!("{}", cyan.apply_to("◆ Available Skins"));
    println!();

    for skin in &skins {
        let is_active = skin.name == active.name;
        let marker = if is_active {
            green.apply_to("●").to_string()
        } else {
            dim.apply_to("○").to_string()
        };
        let name = if is_active {
            green.apply_to(&skin.name).to_string()
        } else {
            skin.name.clone()
        };
        println!(
            "  {} {:15} {}",
            marker,
            name,
            dim.apply_to(&skin.description)
        );
    }

    println!();
    println!(
        "  {} Active: {} {}",
        green.apply_to("→"),
        green.apply_to(&active.name),
        dim.apply_to(&format!("— {}", active.description))
    );
    println!(
        "  {} Use `hermes skin apply <name>` to switch",
        dim.apply_to("→")
    );
    println!();

    Ok(())
}

/// CLI: apply a skin by name.
pub fn cmd_skin_apply(name: &str) -> anyhow::Result<()> {
    use console::Style;

    let green = Style::new().green();
    let dim = Style::new().dim();

    // Validate the skin exists
    let skin = resolve_skin(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Skin '{}' not found. Run `hermes skin list` to see available skins.",
            name
        )
    })?;

    set_active_skin(name)?;

    println!();
    println!(
        "  {} Skin applied: {}",
        green.apply_to("✓"),
        green.apply_to(name)
    );
    println!("    {}", dim.apply_to(&skin.description));

    // Show a preview of key colors
    if !skin.colors.is_empty() {
        println!();
        println!("  {} Color palette:", dim.apply_to("→"));
        for (key, hex) in skin.colors.iter().take(6) {
            if let Some(style) = hex_to_style(hex) {
                println!("    {} {} {}", style.apply_to("██"), key, dim.apply_to(hex));
            } else {
                println!("    ██ {} {}", key, dim.apply_to(hex));
            }
        }
    }

    // Show spinner preview if customized
    if !skin.spinner.waiting_faces.is_empty() {
        println!();
        println!(
            "  {} Spinner faces: {}",
            dim.apply_to("→"),
            skin.spinner.waiting_faces.join(" ")
        );
    }

    println!();
    Ok(())
}

/// CLI: preview a skin without applying.
pub fn cmd_skin_preview(name: &str) -> anyhow::Result<()> {
    use console::Style;

    let cyan = Style::new().cyan();
    let dim = Style::new().dim();
    let yellow = Style::new().yellow();

    let skin = resolve_skin(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Skin '{}' not found. Run `hermes skin list` to see available skins.",
            name
        )
    })?;

    println!();
    println!("{}", cyan.apply_to(format!("◆ Skin Preview: {name}")));
    println!();
    println!("  Name:        {}", skin.name);
    println!("  Description: {}", skin.description);
    println!();

    // Branding preview
    println!("  {}", dim.apply_to("Branding:"));
    for (key, value) in &skin.branding {
        println!("    {:20} {}", key, value);
    }

    // Color swatches
    if !skin.colors.is_empty() {
        println!();
        println!("  {}", dim.apply_to("Colors:"));
        for (key, hex) in &skin.colors {
            if let Some(styled) = hex_to_style(hex) {
                print!("  {} ", styled.apply_to("██"));
            } else {
                print!("  ██ ");
            }
            println!("{:20} {}", key, dim.apply_to(hex));
        }
    }

    // Spinner preview
    if !skin.spinner.waiting_faces.is_empty() {
        println!();
        println!("  {}", dim.apply_to("Spinner waiting faces:"));
        for face in &skin.spinner.waiting_faces {
            print!(" {} ", face);
        }
        println!();
    }
    if !skin.spinner.thinking_faces.is_empty() {
        println!();
        println!("  {}", dim.apply_to("Spinner thinking faces:"));
        for face in &skin.spinner.thinking_faces {
            print!(" {} ", face);
        }
        println!();
    }
    if !skin.spinner.thinking_verbs.is_empty() {
        println!();
        println!(
            "  {} {}",
            dim.apply_to("Spinner verbs:"),
            skin.spinner.thinking_verbs.join(", ")
        );
    }
    if !skin.spinner.wings.is_empty() {
        println!();
        println!("  {}", dim.apply_to("Spinner wings:"));
        for wing in &skin.spinner.wings {
            println!("    {} ... {}", wing[0], wing[1]);
        }
    }

    // Tool prefix
    println!();
    println!(
        "  {} Tool prefix: '{}'",
        dim.apply_to("→"),
        skin.tool_prefix
    );

    // Apply hint
    println!();
    println!(
        "  {} Run `hermes skin apply {}` to activate this skin.",
        yellow.apply_to("→"),
        name
    );
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_default() {
        let skin = get_builtin_skin("default").unwrap();
        assert_eq!(skin.name, "default");
        assert_eq!(skin.get_color("banner_title", ""), "#FFD700");
    }

    #[test]
    fn test_builtin_ares() {
        let skin = get_builtin_skin("ares").unwrap();
        assert_eq!(skin.name, "ares");
        assert!(!skin.spinner.waiting_faces.is_empty());
    }

    #[test]
    fn test_get_branding() {
        let skin = get_builtin_skin("default").unwrap();
        assert_eq!(skin.get_branding("agent_name", ""), "Hermes Agent");
        assert_eq!(skin.get_branding("nonexistent", "fallback"), "fallback");
    }

    #[test]
    fn test_hex_to_style_valid() {
        let style = hex_to_style("#FF5733");
        assert!(style.is_some());
    }

    #[test]
    fn test_hex_to_style_invalid() {
        assert!(hex_to_style("not-a-color").is_none());
        assert!(hex_to_style("").is_none());
    }

    #[test]
    fn test_list_skins() {
        let skins = list_skins();
        assert!(
            skins.iter().any(|s| s.name == "default"),
            "default skin should be listed"
        );
        assert!(
            skins.iter().any(|s| s.name == "ares"),
            "ares skin should be listed"
        );
    }

    #[test]
    fn test_resolve_skin_builtin() {
        let skin = resolve_skin("slate").unwrap();
        assert_eq!(skin.name, "slate");
    }

    #[test]
    fn test_resolve_skin_unknown() {
        assert!(resolve_skin("definitely-not-a-real-skin").is_none());
    }
}
