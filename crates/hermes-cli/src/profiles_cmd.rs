#![allow(dead_code)]
//! Profile management CLI commands.
//!
//! Mirrors the Python `hermes_cli/profiles.py` module.
//! Each profile is an isolated HERMES_HOME directory with its own config,
//! memory, sessions, skills, cron, and gateway state.
//!
//! Profiles live under `~/.hermes/profiles/<name>/`. The "default" profile
//! is `~/.hermes` itself (backward compatible, zero migration needed).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use console::Style;
use hermes_core::{get_default_hermes_root, get_hermes_home};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Valid profile name pattern: lowercase alphanumeric start, then [a-z0-9_-]{0,63}
const PROFILE_NAME_RE: &str = r"^[a-z0-9][a-z0-9_-]{0,63}$";

/// Directories bootstrapped inside every new profile.
const PROFILE_DIRS: &[&str] = &[
    "memories",
    "sessions",
    "skills",
    "skins",
    "logs",
    "plans",
    "workspace",
    "cron",
    "home",
];

/// Files copied during --clone (if they exist in the source).
const CLONE_CONFIG_FILES: &[&str] = &["config.yaml", ".env", "SOUL.md"];

/// Subdirectory files copied during --clone.
const CLONE_SUBDIR_FILES: &[&str] = &["memories/MEMORY.md", "memories/USER.md"];

/// Runtime files stripped after --clone-all.
const CLONE_ALL_STRIP: &[&str] = &["gateway.pid", "gateway_state.json", "processes.json"];

/// Reserved names that cannot be used as profile aliases.
const RESERVED_NAMES: &[&str] = &["hermes", "default", "test", "tmp", "root", "sudo"];

/// Hermes subcommands that cannot be used as profile names/aliases.
const HERMES_SUBCOMMANDS: &[&str] = &[
    "chat", "model", "gateway", "setup", "whatsapp", "login", "logout",
    "status", "cron", "doctor", "dump", "config", "pairing", "skills", "tools",
    "mcp", "sessions", "insights", "version", "update", "uninstall",
    "profile", "plugins", "honcho", "acp",
];

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the directory where named profiles are stored.
fn get_profiles_root() -> PathBuf {
    get_default_hermes_root().join("profiles")
}

/// Return the path to the sticky active_profile file.
fn active_profile_path() -> PathBuf {
    get_default_hermes_root().join("active_profile")
}

/// Return the directory for wrapper scripts.
fn wrapper_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("bin")
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a profile name. Returns an error message if invalid.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name == "default" {
        return Ok(());
    }
    let re = regex::Regex::new(PROFILE_NAME_RE).unwrap();
    if !re.is_match(name) {
        return Err(format!(
            "Invalid profile name '{name}'. Must match [a-z0-9][a-z0-9_-]{{0,63}}"
        ));
    }
    Ok(())
}

/// Resolve a profile name to its directory path.
pub fn get_profile_dir(name: &str) -> PathBuf {
    if name == "default" {
        return get_hermes_home();
    }
    get_profiles_root().join(name)
}

/// Check whether a profile directory exists.
pub fn profile_exists(name: &str) -> bool {
    if name == "default" {
        return true;
    }
    get_profile_dir(name).is_dir()
}

// ---------------------------------------------------------------------------
// Alias / wrapper script management
// ---------------------------------------------------------------------------

/// Check if a profile name would collide with reserved names or existing commands.
fn check_alias_collision(name: &str) -> Option<String> {
    if RESERVED_NAMES.contains(&name) {
        return Some(format!("'{name}' is a reserved name"));
    }
    if HERMES_SUBCOMMANDS.contains(&name) {
        return Some(format!("'{name}' conflicts with a hermes subcommand"));
    }

    let wrapper = wrapper_dir().join(name);
    if wrapper.exists() {
        // Check if it's our own wrapper
        if let Ok(content) = fs::read_to_string(&wrapper) {
            if content.contains("hermes -p") {
                return None; // safe to overwrite
            }
        }
        return Some(format!("'{name}' conflicts with an existing command"));
    }

    // Check if command exists in PATH
    if which::which(name).is_ok() {
        return Some(format!("'{name}' conflicts with an existing command in PATH"));
    }

    None
}

/// Create a shell wrapper script at ~/.local/bin/<name>.
pub fn create_wrapper_script(name: &str) -> Option<PathBuf> {
    let dir = wrapper_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  \u{26A0} Could not create {}: {e}", dir.display());
        return None;
    }

    let wrapper = dir.join(name);
    let content = format!("#!/bin/sh\nexec hermes -p {name} \"$@\"\n");
    if fs::write(&wrapper, content).is_err() {
        eprintln!("  \u{26A0} Could not create wrapper at {}", wrapper.display());
        return None;
    }

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&wrapper) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = fs::set_permissions(&wrapper, perms);
        }
    }

    Some(wrapper)
}

/// Remove the wrapper script for a profile. Returns true if removed.
fn remove_wrapper_script(name: &str) -> bool {
    let wrapper = wrapper_dir().join(name);
    if wrapper.exists() {
        if let Ok(content) = fs::read_to_string(&wrapper) {
            if content.contains("hermes -p") {
                return fs::remove_file(&wrapper).is_ok();
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// ProfileInfo
// ---------------------------------------------------------------------------

/// Summary information about a profile.
pub struct ProfileInfo {
    pub name: String,
    pub path: PathBuf,
    pub is_default: bool,
    pub gateway_running: bool,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub has_env: bool,
    pub skill_count: usize,
    pub alias_path: Option<PathBuf>,
}

/// Read model/provider from a profile's config.yaml.
fn read_config_model(profile_dir: &Path) -> (Option<String>, Option<String>) {
    let config_path = profile_dir.join("config.yaml");
    if !config_path.exists() {
        return (None, None);
    }

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };

    let value: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };

    if let Some(model_val) = value.get("model") {
        match model_val {
            serde_yaml::Value::String(s) => (Some(s.clone()), None),
            serde_yaml::Value::Mapping(m) => {
                let model = m
                    .get("default")
                    .or_else(|| m.get("model"))
                    .and_then(|v| v.as_str().map(String::from));
                let provider = m.get("provider").and_then(|v| v.as_str().map(String::from));
                (model, provider)
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    }
}

/// Check if a gateway is running for a given profile directory.
fn check_gateway_running(profile_dir: &Path) -> bool {
    let pid_file = profile_dir.join("gateway.pid");
    if !pid_file.exists() {
        return false;
    }

    let raw = match fs::read_to_string(&pid_file) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }

    // Parse JSON or plain PID
    let pid: Option<u32> = if raw.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32))
    } else {
        raw.parse::<u32>().ok()
    };

    let Some(pid) = pid else {
        return false;
    };

    // Check if process exists
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }
    #[cfg(not(unix))]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(&pid.to_string())
        } else {
            false
        }
    }
}

/// Count installed skills in a profile.
fn count_skills(profile_dir: &Path) -> usize {
    let skills_dir = profile_dir.join("skills");
    if !skills_dir.is_dir() {
        return 0;
    }

    let mut count = 0;
    if let Ok(entries) = skills_dir.read_dir() {
        count += count_skills_recursive(entries, 0);
    }
    count
}

fn count_skills_recursive(entries: std::fs::ReadDir, depth: usize) -> usize {
    let mut count = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // Skip .hub and .git directories
        if path.file_name().is_some_and(|n| n == ".hub" || n == ".git") {
            continue;
        }
        if path.file_name().is_some_and(|n| n == "SKILL.md") && path.is_file() {
            count += 1;
        }
        if path.is_dir() && depth < 5 {
            if let Ok(sub) = path.read_dir() {
                count += count_skills_recursive(sub, depth + 1);
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Active profile (sticky default)
// ---------------------------------------------------------------------------

/// Read the sticky active profile name. Returns "default" if not set.
pub fn get_active_profile() -> String {
    let path = active_profile_path();
    match fs::read_to_string(&path) {
        Ok(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                "default".to_string()
            } else {
                name
            }
        }
        Err(_) => "default".to_string(),
    }
}

/// Set the sticky active profile. Write "default" to clear.
pub fn set_active_profile(name: &str) -> io::Result<()> {
    let path = active_profile_path();
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;

    if name == "default" {
        let _ = fs::remove_file(&path);
        Ok(())
    } else {
        // Atomic write via temp file
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{name}\n"))?;
        fs::rename(&tmp, &path)
    }
}

// ---------------------------------------------------------------------------
// CRUD operations
// ---------------------------------------------------------------------------

/// Return info for all profiles, including the default.
pub fn list_profiles() -> Vec<ProfileInfo> {
    let mut profiles = Vec::new();
    let wdir = wrapper_dir();

    // Default profile
    let default_home = get_hermes_home();
    if default_home.is_dir() {
        let (model, provider) = read_config_model(&default_home);
        profiles.push(ProfileInfo {
            name: "default".to_string(),
            path: default_home.clone(),
            is_default: true,
            gateway_running: check_gateway_running(&default_home),
            model,
            provider,
            has_env: default_home.join(".env").exists(),
            skill_count: count_skills(&default_home),
            alias_path: None,
        });
    }

    // Named profiles
    let profiles_root = get_profiles_root();
    if profiles_root.is_dir() {
        let re = regex::Regex::new(PROFILE_NAME_RE).unwrap();
        if let Ok(entries) = profiles_root.read_dir() {
            let mut dirs: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            dirs.sort_by_key(|e| e.file_name());

            for entry in dirs {
                let name = entry.file_name().to_string_lossy().to_string();
                if !re.is_match(&name) {
                    continue;
                }
                let profile_dir = entry.path();
                let (model, provider) = read_config_model(&profile_dir);
                let alias_path = wdir.join(&name);
                profiles.push(ProfileInfo {
                    name,
                    path: profile_dir.clone(),
                    is_default: false,
                    gateway_running: check_gateway_running(&profile_dir),
                    model,
                    provider,
                    has_env: profile_dir.join(".env").exists(),
                    skill_count: count_skills(&profile_dir),
                    alias_path: if alias_path.exists() {
                        Some(alias_path)
                    } else {
                        None
                    },
                });
            }
        }
    }

    profiles
}

/// Display the profile list.
pub fn cmd_profile_list() -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();
    let active = get_active_profile();

    println!();
    println!("{}", cyan.apply_to("◆ Profiles"));
    println!();

    let default_home = get_hermes_home();
    let profiles_root = get_profiles_root();
    println!("  HERMES_HOME: {}", display_path(&default_home));
    println!("  Profiles:    {}", display_path(&profiles_root));
    println!("  Active:      {active}");
    println!();

    let profiles = list_profiles();
    if profiles.is_empty() {
        println!("  {}", dim.apply_to("No profiles found."));
        println!();
        return Ok(());
    }

    // Find longest name for alignment
    let max_name_len = profiles.iter().map(|p| p.name.len()).max().unwrap_or(7);

    for profile in &profiles {
        let marker = if profile.name == active {
            green.apply_to("*").to_string()
        } else {
            " ".to_string()
        };
        let name_padded = format!("{:<width$}", profile.name, width = max_name_len);
        let tag = if profile.is_default {
            dim.apply_to(" (default)").to_string()
        } else {
            String::new()
        };

        println!("  {marker} {name_padded}{tag}");
        println!("     {}", dim.apply_to(&display_path(&profile.path)));

        if let Some(model) = &profile.model {
            let provider_str = profile
                .provider
                .as_ref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            println!("     model: {model}{provider_str}");
        }
        if profile.has_env {
            println!("     .env: yes");
        }
        if profile.skill_count > 0 {
            println!("     skills: {}", profile.skill_count);
        }
        if profile.gateway_running {
            println!("     gateway: {}", yellow.apply_to("running"));
        }
        if let Some(alias) = &profile.alias_path {
            println!("     alias: {}", display_path(alias));
        }
        println!();
    }

    Ok(())
}

/// Create a new profile.
pub fn cmd_profile_create(
    name: &str,
    clone: bool,
    clone_all: bool,
    clone_from: Option<&str>,
    no_alias: bool,
) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let cyan = Style::new().cyan();

    // Validate name
    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;
    if name == "default" {
        anyhow::bail!("Cannot create a profile named 'default' — it is the built-in profile (~/.hermes).");
    }

    let profile_dir = get_profile_dir(name);
    if profile_dir.exists() {
        println!(
            "  {} Profile '{name}' already exists at: {}",
            yellow.apply_to("\u{26A0}"),
            profile_dir.display()
        );
        return Ok(());
    }

    // Resolve clone source
    let source_dir = if clone || clone_all || clone_from.is_some() {
        let src = match clone_from {
            Some(src_name) => get_profile_dir(src_name),
            None => get_hermes_home(),
        };
        if !src.is_dir() {
            let label = clone_from.unwrap_or("active");
            anyhow::bail!("Source profile '{label}' does not exist at {}", src.display());
        }
        Some(src)
    } else {
        None
    };

    if clone_all {
        if let Some(ref src) = source_dir {
            copy_dir_all(src, &profile_dir)?;
            // Strip runtime files
            for stale in CLONE_ALL_STRIP {
                let _ = fs::remove_file(profile_dir.join(stale));
            }
        }
    } else {
        // Bootstrap directory structure
        fs::create_dir_all(&profile_dir)?;
        for subdir in PROFILE_DIRS {
            fs::create_dir_all(profile_dir.join(subdir))?;
        }

        // Clone config files from source
        if let Some(ref src) = source_dir {
            for filename in CLONE_CONFIG_FILES {
                let src_file = src.join(filename);
                if src_file.exists() {
                    let _ = fs::copy(&src_file, profile_dir.join(filename));
                }
            }

            // Clone memory and other subdirectory files
            for relpath in CLONE_SUBDIR_FILES {
                let src_file = src.join(relpath);
                if src_file.exists() {
                    if let Some(parent) = profile_dir.join(relpath).parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let _ = fs::copy(&src_file, profile_dir.join(relpath));
                }
            }
        }
    }

    // Seed a default SOUL.md if not present
    let soul_path = profile_dir.join("SOUL.md");
    if !soul_path.exists() {
        let _ = fs::write(&soul_path, DEFAULT_SOUL_MD);
    }

    println!(
        "  {} Profile '{name}' created at: {}",
        green.apply_to("\u{2713}"),
        profile_dir.display()
    );

    // Create wrapper script (alias)
    if !no_alias {
        if let Some(collision) = check_alias_collision(name) {
            println!(
                "  {} Cannot create alias '{name}' — {collision}",
                yellow.apply_to("\u{26A0}")
            );
        } else if let Some(wrapper) = create_wrapper_script(name) {
            println!(
                "  {} Alias created: {}",
                green.apply_to("\u{2713}"),
                wrapper.display()
            );
        }
    }

    println!();
    println!(
        "  {}",
        cyan.apply_to("Set HERMES_HOME to switch profiles:")
    );
    println!(
        "    {}",
        cyan.apply_to(format!("  HERMES_HOME={} hermes", profile_dir.display()))
    );
    println!();

    Ok(())
}

/// Delete a profile with confirmation.
pub fn cmd_profile_delete(name: &str, force: bool) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;

    if name == "default" {
        anyhow::bail!(
            "Cannot delete the default profile (~/.hermes).\n\
             To remove everything, use: hermes uninstall"
        );
    }

    let profile_dir = get_profile_dir(name);
    if !profile_dir.is_dir() {
        anyhow::bail!("Profile '{name}' does not exist.");
    }

    // Show what will be deleted
    let (model, provider) = read_config_model(&profile_dir);
    let gw_running = check_gateway_running(&profile_dir);
    let skill_count = count_skills(&profile_dir);
    let wrapper_path = wrapper_dir().join(name);
    let has_wrapper = wrapper_path.exists();

    println!();
    println!("Profile: {name}");
    println!("Path:    {}", profile_dir.display());
    if let Some(m) = model {
        let provider_str = provider.map(|p| format!(" ({p})")).unwrap_or_default();
        println!("Model:   {m}{provider_str}");
    }
    if skill_count > 0 {
        println!("Skills:  {skill_count}");
    }
    println!();
    println!("This will permanently delete:");
    println!("  All config, API keys, memories, sessions, skills, cron jobs");
    if has_wrapper {
        println!("  Command alias ({})", wrapper_path.display());
    }
    if gw_running {
        println!("  {} Gateway is running — it will be stopped.", yellow.apply_to("\u{26A0}"));
    }
    println!();

    // Confirmation
    if !force {
        print!("Type '{name}' to confirm: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let confirm = input.trim();
        if confirm != name {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Stop gateway if running
    if gw_running {
        stop_gateway_process(&profile_dir);
    }

    // Remove wrapper script
    if has_wrapper
        && remove_wrapper_script(name) {
            println!("  {} Removed {}", green.apply_to("\u{2713}"), wrapper_path.display());
        }

    // Remove profile directory
    fs::remove_dir_all(&profile_dir)?;
    println!("  {} Removed {}", green.apply_to("\u{2713}"), profile_dir.display());

    // Clear active_profile if it pointed to this profile
    if get_active_profile() == name {
        set_active_profile("default")?;
        println!("  {} Active profile reset to default", green.apply_to("\u{2713}"));
    }

    println!();
    println!("Profile '{name}' deleted.");
    println!();

    Ok(())
}

/// Show profile details.
pub fn cmd_profile_show(name: &str) -> anyhow::Result<()> {
    let cyan = Style::new().cyan();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;

    if name == "default" {
        let home = get_hermes_home();
        println!();
        println!("{}", cyan.apply_to(format!("◆ Profile: {name}")));
        println!("  Path:    {}", display_path(&home));
        println!("  Type:    default (built-in)");
        println!("  HERMES_HOME: {}", display_path(&home));
        println!();

        if home.join("config.yaml").exists() {
            println!("  Config:  present");
        } else {
            println!("  {}", dim.apply_to("Config: not found — run 'hermes setup' to configure"));
        }
        if home.join(".env").exists() {
            println!("  .env:    present");
        }
        if home.join("SOUL.md").exists() {
            println!("  SOUL.md: present");
        }
        println!();

        return Ok(());
    }

    let profile_dir = get_profile_dir(name);
    if !profile_dir.exists() {
        println!(
            "  {} Profile '{name}' not found. Create it with: hermes profile create {name}",
            yellow.apply_to("\u{2717}")
        );
        return Ok(());
    }

    let (model, provider) = read_config_model(&profile_dir);
    let gw_running = check_gateway_running(&profile_dir);
    let skill_count = count_skills(&profile_dir);
    let wrapper_path = wrapper_dir().join(name);
    let has_wrapper = wrapper_path.exists();

    println!();
    println!("{}", cyan.apply_to(format!("◆ Profile: {name}")));
    println!("  Path:    {}", display_path(&profile_dir));
    println!("  Type:    named");

    if let Some(m) = model {
        let provider_str = provider.map(|p| format!(" ({p})")).unwrap_or_default();
        println!("  Model:   {m}{provider_str}");
    }
    println!("  Skills:  {skill_count}");
    if gw_running {
        println!("  Gateway: {}", yellow.apply_to("running"));
    } else {
        println!("  Gateway: stopped");
    }

    if has_wrapper {
        println!("  Alias:   {}", display_path(&wrapper_path));
    }

    println!();

    // Subdirectory status
    let dirs_to_check = ["memories", "sessions", "skills", "cron"];
    for d in &dirs_to_check {
        let dir = profile_dir.join(d);
        if dir.is_dir() {
            let count = dir.read_dir().map(|e| e.count()).unwrap_or(0);
            println!("  {d:12} {count} item(s)");
        } else {
            println!("  {d:12} {}", dim.apply_to("not found"));
        }
    }

    println!();

    Ok(())
}

/// Manage profile alias / wrapper script.
pub fn cmd_profile_alias(
    name: &str,
    target: Option<&str>,
    remove: bool,
) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let dim = Style::new().dim();

    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;

    if name == "default" {
        anyhow::bail!("Cannot create an alias for the default profile.");
    }

    let profile_dir = get_profile_dir(name);
    if !profile_dir.exists() {
        anyhow::bail!("Profile '{name}' does not exist. Create it first.");
    }

    if remove {
        if remove_wrapper_script(name) {
            println!(
                "  {} Alias for '{name}' removed.",
                green.apply_to("\u{2713}")
            );
        } else {
            println!(
                "  {} No alias found for '{name}'.",
                yellow.apply_to("\u{26A0}")
            );
        }
        println!();
        return Ok(());
    }

    // If target is provided, create alias pointing to a different profile
    let alias_name = target.unwrap_or(name);
    if let Some(collision) = check_alias_collision(alias_name) {
        anyhow::bail!("Cannot create alias '{alias_name}' — {collision}");
    }

    if let Some(wrapper) = create_profile_alias(alias_name, name) {
        println!(
            "  {} Alias '{alias_name}' -> profile '{name}' created",
            green.apply_to("\u{2713}")
        );
        println!("     {}", display_path(&wrapper));
        println!();
        println!(
            "  {}",
            dim.apply_to("Note: If ~/.local/bin is not in your PATH, add it:")
        );
        println!("    export PATH=\"$HOME/.local/bin:$PATH\"");
        println!();
    }

    Ok(())
}

/// Create an alias wrapper for a profile.
fn create_profile_alias(alias_name: &str, profile_name: &str) -> Option<PathBuf> {
    let dir = wrapper_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  \u{26A0} Could not create {}: {e}", dir.display());
        return None;
    }

    let wrapper = dir.join(alias_name);
    let content = format!("#!/bin/sh\nexec hermes -p {profile_name} \"$@\"\n");
    if fs::write(&wrapper, content).is_err() {
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&wrapper) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = fs::set_permissions(&wrapper, perms);
        }
    }

    Some(wrapper)
}

/// Rename a profile.
pub fn cmd_profile_rename(old_name: &str, new_name: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    validate_profile_name(old_name).map_err(|e| anyhow::anyhow!("{}", e))?;
    validate_profile_name(new_name).map_err(|e| anyhow::anyhow!("{}", e))?;

    if old_name == "default" {
        anyhow::bail!("Cannot rename the default profile.");
    }
    if new_name == "default" {
        anyhow::bail!("Cannot rename to 'default' — it is reserved.");
    }

    let old_dir = get_profile_dir(old_name);
    let new_dir = get_profile_dir(new_name);

    if !old_dir.is_dir() {
        anyhow::bail!("Profile '{old_name}' does not exist.");
    }
    if new_dir.exists() {
        anyhow::bail!("Profile '{new_name}' already exists.");
    }

    // Stop gateway if running
    if check_gateway_running(&old_dir) {
        stop_gateway_process(&old_dir);
    }

    // Rename directory
    fs::rename(&old_dir, &new_dir)?;
    println!(
        "  {} Renamed {} -> {}",
        green.apply_to("\u{2713}"),
        old_name,
        new_name
    );

    // Update wrapper script
    remove_wrapper_script(old_name);
    if let Some(collision) = check_alias_collision(new_name) {
        println!(
            "  {} Cannot create alias '{new_name}' — {collision}",
            yellow.apply_to("\u{26A0}")
        );
    } else if let Some(wrapper) = create_wrapper_script(new_name) {
        println!(
            "  {} Alias updated: {}",
            green.apply_to("\u{2713}"),
            wrapper.display()
        );
    }

    // Update active_profile if it pointed to old name
    if get_active_profile() == old_name {
        set_active_profile(new_name)?;
        println!(
            "  {} Active profile updated: {new_name}",
            green.apply_to("\u{2713}")
        );
    }

    println!();

    Ok(())
}

/// Export a profile to a tar.gz archive.
pub fn cmd_profile_export(name: &str, output: Option<&str>) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;

    let profile_dir = get_profile_dir(name);
    if !profile_dir.is_dir() {
        anyhow::bail!("Profile '{name}' does not exist.");
    }

    let default_out = format!("{name}.tar.gz");
    let out_path = output.unwrap_or(&default_out);

    // Check for tar availability
    if !tar_available() {
        // Fallback: manual zip
        println!(
            "  {}",
            yellow.apply_to("Note: tar not available, export may not work as expected.")
        );
    }

    println!("  Exporting profile '{name}' to {out_path}...");

    let profiles_root = get_profiles_root();
    let default_root = get_default_hermes_root();

    let result = if name == "default" {
        // Default profile: exclude infrastructure files
        export_default_profile(&default_root, out_path)
    } else {
        // Named profile: simple tar
        #[cfg(unix)]
        {
            std::process::Command::new("tar")
                .args(["-czf", out_path, "-C", &profiles_root.to_string_lossy(), name])
                .output()
        }
        #[cfg(not(unix))]
        {
            std::process::Command::new("tar")
                .args(["-cf", out_path, "-C", &profiles_root.to_string_lossy(), name])
                .output()
        }
    };

    match result {
        Ok(out) if out.status.success() => {
            println!(
                "  {} Exported to: {out_path}",
                green.apply_to("\u{2713}")
            );
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            println!(
                "  {} Export failed: {err}",
                yellow.apply_to("\u{26A0}")
            );
        }
        Err(e) => {
            println!("  {} Failed: {e}", yellow.apply_to("\u{26A0}"));
        }
    }

    Ok(())
}

/// Check if tar is available.
fn tar_available() -> bool {
    std::process::Command::new("tar")
        .arg("--version")
        .output()
        .is_ok()
}

/// Export default profile excluding infrastructure files.
fn export_default_profile(root: &Path, out_path: &str) -> io::Result<std::process::Output> {
    // Build exclude args
    let exclude_root: &[&str] = &[
        "hermes-agent", ".worktrees", "profiles", "bin", "node_modules",
        "state.db", "state.db-shm", "state.db-wal", "hermes_state.db",
        "response_store.db", "response_store.db-shm", "response_store.db-wal",
        "gateway.pid", "gateway_state.json", "processes.json",
        "auth.json", ".env", "auth.lock", "active_profile",
        ".update_check", "errors.log", ".hermes_history",
        "image_cache", "audio_cache", "document_cache",
        "browser_screenshots", "checkpoints", "sandboxes", "logs",
    ];

    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-czf").arg(out_path);

    for excl in exclude_root {
        cmd.arg("--exclude").arg(excl);
    }

    // Also exclude __pycache__ and temp files at any level
    cmd.arg("--exclude=__pycache__");
    cmd.arg("--exclude=*.sock");
    cmd.arg("--exclude=*.tmp");

    cmd.arg("-C")
        .arg(root.parent().unwrap_or(root))
        .arg(root.file_name().unwrap_or_default());

    cmd.output()
}

/// Import a profile from a tar.gz archive.
pub fn cmd_profile_import(path: &str, name: Option<&str>) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let archive = Path::new(path);
    if !archive.exists() {
        anyhow::bail!("Archive not found: {path}");
    }

    let profiles_root = get_profiles_root();
    fs::create_dir_all(&profiles_root)?;

    // Try to infer name from archive if not provided
    let inferred_name: Option<String> = name.map(String::from).or_else(|| {
        // Peek at archive top-level dir
        #[cfg(unix)]
        {
            let output = std::process::Command::new("tar")
                .args(["-tzf", path])
                .output()
                .ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Get first path component
            stdout
                .lines()
                .next()
                .map(|line| line.split('/').next().unwrap_or("").to_string())
                .filter(|s| !s.is_empty())
        }
        #[cfg(not(unix))]
        {
            None
        }
    });

    let profile_name = inferred_name
        .as_deref()
        .unwrap_or_else(|| name.unwrap_or("imported"))
        .to_string();

    if profile_name == "default" {
        anyhow::bail!(
            "Cannot import as 'default' — that is the built-in root profile (~/.hermes). \
             Specify a different name."
        );
    }

    validate_profile_name(&profile_name)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let profile_dir = get_profile_dir(&profile_name);
    if profile_dir.exists() {
        anyhow::bail!("Profile '{profile_name}' already exists at {}", profile_dir.display());
    }

    // Extract archive
    #[cfg(unix)]
    let output = std::process::Command::new("tar")
        .args(["-xf", path, "-C", &profiles_root.to_string_lossy()])
        .output();
    #[cfg(not(unix))]
    let output = std::process::Command::new("tar")
        .args(["-xf", path, "-C", &profiles_root.to_string_lossy()])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // If archive extracted under different name, rename
            if let Some(ref inferred) = inferred_name {
                let extracted_dir = profiles_root.join(inferred);
                if extracted_dir.exists() && extracted_dir != profile_dir {
                    let _ = fs::rename(&extracted_dir, &profile_dir);
                }
            }
            println!(
                "  {} Profile imported to: {}",
                green.apply_to("\u{2713}"),
                profile_dir.display()
            );
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err_trimmed = err.trim();
            println!("  {} Import failed: {err_trimmed}", yellow.apply_to("\u{26A0}"));
        }
        Err(e) => {
            println!("  {} Failed: {e}", yellow.apply_to("\u{26A0}"));
        }
    }

    Ok(())
}

/// Switch to a profile (sets HERMES_HOME).
pub fn cmd_profile_use(name: &str) -> anyhow::Result<()> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let cyan = Style::new().cyan();

    validate_profile_name(name).map_err(|e| anyhow::anyhow!("{}", e))?;

    let profile_dir = get_profile_dir(name);
    if !profile_dir.exists() {
        println!(
            "  {} Profile '{name}' not found. Create it first with: hermes profile create {name}",
            yellow.apply_to("\u{2717}")
        );
        return Ok(());
    }

    // Set as sticky active profile
    set_active_profile(name)?;

    println!(
        "  {} Active profile set to: {name}",
        green.apply_to("\u{2713}")
    );
    println!();
    println!(
        "  {}",
        cyan.apply_to("To use this profile in the current session, set:")
    );
    println!(
        "    {}",
        green.apply_to(format!("HERMES_HOME={}", profile_dir.display()))
    );
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Gateway helpers
// ---------------------------------------------------------------------------

/// Stop a running gateway process via its PID file.
fn stop_gateway_process(profile_dir: &Path) {
    let pid_file = profile_dir.join("gateway.pid");
    if !pid_file.exists() {
        return;
    }

    let raw = match fs::read_to_string(&pid_file) {
        Ok(r) => r,
        Err(_) => return,
    };
    let raw = raw.trim();

    let pid: Option<u32> = if raw.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32))
    } else {
        raw.parse::<u32>().ok()
    };

    let Some(pid) = pid else {
        return;
    };

    #[cfg(unix)]
    {
        unsafe {
            if libc::kill(pid as i32, libc::SIGTERM) == 0 {
                // Wait up to 10s for graceful shutdown
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    if libc::kill(pid as i32, 0) != 0 {
                        println!("  \u{2713} Gateway stopped (PID {pid})");
                        return;
                    }
                }
                // Force kill
                let _ = libc::kill(pid as i32, libc::SIGKILL);
                println!("  \u{2713} Gateway force-stopped (PID {pid})");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
        println!("  \u{2713} Gateway stopped (PID {pid})");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Copy a directory recursively.
fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Format a path with ~ substitution for display.
fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// Default SOUL.md content
// ---------------------------------------------------------------------------

const DEFAULT_SOUL_MD: &str = r#"# SOUL.md — Your Identity

You are Hermes, a helpful, capable AI assistant. You have access to tools
that let you execute CLI commands on my computer, search the web, browse
the internet, and interact with external services. Use them when helpful.

## Tone
- Be warm, conversational, and direct
- Use natural language — no robotic phrasing
- Keep responses concise unless depth is requested

## Approach
- Think step-by-step for complex problems
- Show your reasoning when it adds value
- Be proactive — suggest next steps and improvements
- Admit uncertainty and ask clarifying questions when needed
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_profile_name_valid() {
        assert!(validate_profile_name("coder").is_ok());
        assert!(validate_profile_name("my-profile").is_ok());
        assert!(validate_profile_name("profile_1").is_ok());
        assert!(validate_profile_name("a").is_ok());
        assert!(validate_profile_name("default").is_ok());
    }

    #[test]
    fn test_validate_profile_name_invalid() {
        assert!(validate_profile_name("Agent").is_err()); // uppercase
        assert!(validate_profile_name("-start").is_err()); // starts with -
        assert!(validate_profile_name("_start").is_err()); // starts with _
        assert!(validate_profile_name("").is_err()); // empty
    }

    #[test]
    fn test_check_alias_collision_reserved() {
        assert!(check_alias_collision("default").is_some());
        assert!(check_alias_collision("hermes").is_some());
        assert!(check_alias_collision("chat").is_some());
    }

    #[test]
    fn test_read_config_model_missing() {
        let (model, provider) = read_config_model(Path::new("/nonexistent"));
        assert!(model.is_none());
        assert!(provider.is_none());
    }

    #[test]
    fn test_copy_dir_all() {
        let tmp = std::env::temp_dir();
        let src = tmp.join("profile_test_src");
        let dst = tmp.join("profile_test_dst");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);

        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file.txt"), "hello").unwrap();
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("nested.txt"), "world").unwrap();

        copy_dir_all(&src, &dst).unwrap();
        assert!(dst.join("file.txt").exists());
        assert!(dst.join("subdir").join("nested.txt").exists());
        assert_eq!(fs::read_to_string(dst.join("file.txt")).unwrap(), "hello");

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn test_display_path_home() {
        if let Some(home) = dirs::home_dir() {
            let path = home.join(".hermes/profiles/test");
            let display = display_path(&path);
            assert!(display.starts_with("~/"));
        }
    }

    #[test]
    fn test_display_path_outside_home() {
        let path = Path::new("/opt/data/profiles/test");
        let display = display_path(path);
        assert_eq!(display, "/opt/data/profiles/test");
    }
}
