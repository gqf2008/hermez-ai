#![allow(dead_code)]
//! Gateway service management — start, stop, status, install, uninstall.
//!
//! Mirrors the Python `hermes_cli/gateway.py` service management functions.
//! Supports systemd (Linux), launchd (macOS), and Windows Task Scheduler.

use std::io::Write;

use console::Style;

// ---------------------------------------------------------------------------
// Platform detection
// ---------------------------------------------------------------------------

fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

fn has_systemd() -> bool {
    is_linux() && which::which("systemctl").is_ok()
}

fn has_launchctl() -> bool {
    is_macos() && which::which("launchctl").is_ok()
}

// ---------------------------------------------------------------------------
// Service name and paths
// ---------------------------------------------------------------------------

const SERVICE_BASE: &str = "hermes-gateway";

fn get_service_name() -> String {
    let home = hermes_core::get_hermes_home();
    let default = dirs::home_dir().map(|d| d.join(".hermes")).unwrap_or_default();
    if home == default {
        return SERVICE_BASE.to_string();
    }
    // Check if it's a profile directory
    let profiles_dir = default.join("profiles");
    if let Ok(rel) = home.strip_prefix(&profiles_dir) {
        if rel.components().count() == 1 {
            if let Some(name) = rel.file_name().and_then(|n| n.to_str()) {
                if name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') && name.len() <= 64 {
                    return format!("{SERVICE_BASE}-{name}");
                }
            }
        }
    }
    // Fallback: just the base name (profile scoping via HERMES_HOME in env)
    SERVICE_BASE.to_string()
}

fn systemd_unit_path() -> Option<std::path::PathBuf> {
    let name = get_service_name();
    dirs::home_dir().map(|home| home.join(".config/systemd/user").join(format!("{name}.service")))
}

fn launchd_plist_path() -> std::path::PathBuf {
    let name = get_service_name();
    if let Some(home) = dirs::home_dir() {
        return home.join("Library/LaunchAgents").join(format!("{name}.plist"));
    }
    std::path::PathBuf::new()
}

fn pid_file_path() -> std::path::PathBuf {
    hermes_core::get_hermes_home().join("gateway.pid")
}

// ---------------------------------------------------------------------------
// Executable path resolution
// ---------------------------------------------------------------------------

fn hermes_exe_path() -> String {
    // Try to find the `hermes` binary
    if let Ok(path) = which::which("hermes") {
        return path.to_string_lossy().to_string();
    }
    // Fallback to current exe
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "hermes".to_string())
}

// ---------------------------------------------------------------------------
// systemd management
// ---------------------------------------------------------------------------

fn run_systemctl(args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.arg("--user");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output().map_err(|e| format!("systemctl failed: {e}"))
}

fn install_systemd_service() -> Result<(), String> {
    let name = get_service_name();
    let unit_path = systemd_unit_path()
        .ok_or("Failed to resolve systemd unit path")?;
    let exe = hermes_exe_path();
    let hermes_home = hermes_core::get_hermes_home();

    let unit_content = format!(
        "[Unit]\n\
Description=Hermes Agent Gateway - Messaging Platform Integration\n\
After=network.target\n\n\
[Service]\n\
Type=simple\n\
ExecStart={exe} gateway\n\
Restart=on-failure\n\
RestartSec=10\n\
Environment=HERMES_HOME={}\n\n\
[Install]\n\
WantedBy=default.target\n",
        hermes_home.display(),
    );

    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create systemd directory: {e}"))?;
    }

    let mut file = std::fs::File::create(&unit_path)
        .map_err(|e| format!("Failed to create unit file: {e}"))?;
    file.write_all(unit_content.as_bytes())
        .map_err(|e| format!("Failed to write unit file: {e}"))?;

    // Reload daemon and enable
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", &name])?;

    Ok(())
}

fn uninstall_systemd_service() -> Result<(), String> {
    let name = get_service_name();
    let unit_path = systemd_unit_path();

    // Stop and disable first
    let _ = run_systemctl(&["stop", &name]);
    let _ = run_systemctl(&["disable", &name]);
    let _ = run_systemctl(&["daemon-reload"]);

    // Remove unit file
    if let Some(ref path) = unit_path {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| format!("Failed to remove unit file: {e}"))?;
        }
    }

    // Also clean up system-level if it exists
    let system_path = std::path::Path::new("/etc/systemd/system")
        .join(format!("{name}.service"));
    if system_path.exists() {
        let _ = std::fs::remove_file(&system_path);
    }

    Ok(())
}

fn systemctl_status() -> Result<String, String> {
    let name = get_service_name();
    let output = run_systemctl(&["is-active", &name])?;
    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if status == "active" {
        return Ok("active".to_string());
    }

    // Check if PID file exists for manual gateway
    let pid_file = pid_file_path();
    if pid_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                // Check if process is still running
                let running = is_process_running(pid);
                if running {
                    return Ok(format!("manual (PID {pid})"));
                }
            }
        }
    }

    Ok(format!("inactive (systemctl: {status})"))
}

// ---------------------------------------------------------------------------
// launchd management (macOS)
// ---------------------------------------------------------------------------

fn install_launchd_service() -> Result<(), String> {
    let name = get_service_name();
    let plist_path = launchd_plist_path();
    let exe = hermes_exe_path();
    let hermes_home = hermes_core::get_hermes_home();

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{name}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>gateway</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HERMES_HOME</key>
        <string>{hermes_home}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{hermes_home}/logs/gateway.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{hermes_home}/logs/gateway.stderr.log</string>
</dict>
</plist>
"#,
        hermes_home = hermes_home.display(),
    );

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create LaunchAgents directory: {e}"))?;
    }

    let mut file = std::fs::File::create(&plist_path)
        .map_err(|e| format!("Failed to create plist file: {e}"))?;
    file.write_all(plist_content.as_bytes())
        .map_err(|e| format!("Failed to write plist file: {e}"))?;

    // Load the service
    let output = std::process::Command::new("launchctl")
        .arg("load")
        .arg(&plist_path)
        .output()
        .map_err(|e| format!("launchctl load failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("launchctl load failed: {stderr}"));
    }

    Ok(())
}

fn uninstall_launchd_service() -> Result<(), String> {
    let plist_path = launchd_plist_path();

    // Unload first
    let _ = std::process::Command::new("launchctl")
        .arg("unload")
        .arg(&plist_path)
        .output();

    // Remove plist
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .map_err(|e| format!("Failed to remove plist file: {e}"))?;
    }

    Ok(())
}

fn launchd_status() -> Result<String, String> {
    let name = get_service_name();
    let output = std::process::Command::new("launchctl")
        .arg("list")
        .arg(&name)
        .output()
        .map_err(|e| format!("launchctl list failed: {e}"))?;

    if output.status.success() {
        return Ok("active".to_string());
    }

    // Check PID file for manual gateway
    let pid_file = pid_file_path();
    if pid_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                if is_process_running(pid) {
                    return Ok(format!("manual (PID {pid})"));
                }
            }
        }
    }

    Ok("inactive".to_string())
}

// ---------------------------------------------------------------------------
// Windows (background process via PID file)
// ---------------------------------------------------------------------------

fn start_windows_background() -> Result<(), String> {
    let exe = hermes_exe_path();
    let hermes_home = hermes_core::get_hermes_home();
    let log_dir = hermes_home.join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let stdout_log = log_dir.join("gateway.stdout.log");
    let stderr_log = log_dir.join("gateway.stderr.log");

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("gateway");
    cmd.env("HERMES_HOME", &hermes_home);
    cmd.stdout(std::fs::File::create(&stdout_log)
        .map_err(|e| format!("Failed to create stdout log: {e}"))?);
    cmd.stderr(std::fs::File::create(&stderr_log)
        .map_err(|e| format!("Failed to create stderr log: {e}"))?);

    let child = cmd.spawn()
        .map_err(|e| format!("Failed to start gateway: {e}"))?;

    let pid = child.id();
    std::fs::write(pid_file_path(), pid.to_string())
        .map_err(|e| format!("Failed to write PID file: {e}"))?;

    Ok(())
}

fn stop_windows_background() -> Result<bool, String> {
    let pid_file = pid_file_path();
    if !pid_file.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&pid_file)
        .map_err(|e| format!("Failed to read PID file: {e}"))?;
    let pid = content.trim().parse::<u32>()
        .map_err(|_| "Invalid PID in pid file")?;

    if !is_process_running(pid) {
        let _ = std::fs::remove_file(&pid_file);
        return Ok(false);
    }

    // Send SIGTERM equivalent on Windows
    let output = std::process::Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("Failed to kill process: {e}"))?;

    if output.status.success() {
        let _ = std::fs::remove_file(&pid_file);
        Ok(true)
    } else {
        Err(format!("Failed to stop gateway: {}", String::from_utf8_lossy(&output.stderr)))
    }
}

// ---------------------------------------------------------------------------
// Process check
// ---------------------------------------------------------------------------

fn is_process_running(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("tasklist")
            .arg("/FI")
            .arg(format!("PID eq {pid}"))
            .arg("/NH")
            .output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(&pid.to_string())
        } else {
            false
        }
    }

    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start gateway in foreground (already implemented in app.rs).
/// Start gateway as background service.
pub fn cmd_gateway_start(_all: bool, _system: bool) -> Result<(), String> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    if has_systemd() {
        install_systemd_service()?;
        run_systemctl(&["start", &get_service_name()])?;
        println!("  {} Gateway started via systemd", green.apply_to("✓"));
        println!("    systemctl --user status {}", get_service_name());
    } else if has_launchctl() {
        install_launchd_service()?;
        println!("  {} Gateway started via launchd", green.apply_to("✓"));
        println!("    launchctl list {}", get_service_name());
    } else if is_windows() {
        start_windows_background()?;
        println!("  {} Gateway started in background", green.apply_to("✓"));
        println!("    PID file: {}", pid_file_path().display());
    } else {
        println!("  {} No service manager detected. Starting in background with nohup.", yellow.apply_to("→"));
        // Fallback: nohup-style background
        let exe = hermes_exe_path();
        let hermes_home = hermes_core::get_hermes_home();
        let log_dir = hermes_home.join("logs");
        std::fs::create_dir_all(&log_dir).ok();

        let mut cmd = std::process::Command::new("nohup");
        cmd.arg(&exe);
        cmd.arg("gateway");
        cmd.env("HERMES_HOME", &hermes_home);
        if let Ok(file) = std::fs::File::create(log_dir.join("gateway.stdout.log")) {
            cmd.stdout(file);
        }
        if let Ok(file) = std::fs::File::create(log_dir.join("gateway.stderr.log")) {
            cmd.stderr(file);
        }

        let child = cmd.spawn()
            .map_err(|e| format!("Failed to start gateway: {e}"))?;

        let pid = child.id();
        std::fs::write(pid_file_path(), pid.to_string()).ok();
        println!("  {} Gateway started (PID {pid})", green.apply_to("✓"));
    }

    Ok(())
}

/// Stop gateway service.
pub fn cmd_gateway_stop(_all: bool, _system: bool) -> Result<(), String> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    let mut stopped = false;

    if has_systemd() {
        let name = get_service_name();
        let _ = run_systemctl(&["stop", &name]);
        println!("  {} Gateway service stopped (systemd)", green.apply_to("✓"));
        stopped = true;
    } else if has_launchctl() {
        let _ = uninstall_launchd_service();
        println!("  {} Gateway service unloaded (launchd)", green.apply_to("✓"));
        stopped = true;
    }

    // Also kill manual background process if running
    if is_windows() {
        if let Ok(killed) = stop_windows_background() {
            if killed {
                println!("  {} Background gateway stopped", green.apply_to("✓"));
                stopped = true;
            }
        }
    } else {
        // Check PID file for manual gateway
        let pid_file = pid_file_path();
        if pid_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    if is_process_running(pid) {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(pid as i32, libc::SIGTERM);
                        }
                        // Wait for process to exit
                        for _ in 0..10 {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            if !is_process_running(pid) {
                                break;
                            }
                        }
                        if !is_process_running(pid) {
                            println!("  {} Background gateway stopped (PID {pid})", green.apply_to("✓"));
                            stopped = true;
                        } else {
                            println!("  {} Background gateway did not stop (PID {pid})", yellow.apply_to("⚠"));
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&pid_file);
        }
    }

    if !stopped {
        println!("  {} No running gateway found", yellow.apply_to("→"));
    }

    Ok(())
}

/// Show gateway status.
pub fn cmd_gateway_status(_deep: bool, _system: bool) -> Result<(), String> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();
    let red = Style::new().red();
    let cyan = Style::new().cyan();
    let dim = Style::new().dim();

    println!();
    println!("{}", cyan.apply_to("Gateway Status"));
    println!();

    let status = if has_systemd() {
        systemctl_status()?
    } else if has_launchctl() {
        launchd_status()?
    } else if is_windows() {
        // Check PID file
        let pid_file = pid_file_path();
        if pid_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    if is_process_running(pid) {
                        format!("running (PID {pid})")
                    } else {
                        "stopped (stale PID file)".to_string()
                    }
                } else {
                    "stopped".to_string()
                }
            } else {
                "stopped".to_string()
            }
        } else {
            "not installed".to_string()
        }
    } else {
        systemctl_status().or_else(|_| -> Result<String, String> {
            // Fallback: check PID file
            let pid_file = pid_file_path();
            if pid_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = content.trim().parse::<u32>() {
                        if is_process_running(pid) {
                            return Ok(format!("manual (PID {pid})"));
                        }
                    }
                }
            }
            Ok("not running".to_string())
        })?
    };

    let status_style = if status.starts_with("active") || status.starts_with("running") || status.starts_with("manual") {
        green.clone()
    } else if status.contains("not") || status.contains("stopped") || status.contains("inactive") {
        yellow.clone()
    } else {
        red
    };

    println!("  Service: {}", status_style.apply_to(&status));
    println!("  HERMES_HOME: {}", hermes_core::get_hermes_home().display());
    println!();

    // Show configured platforms
    let hermes_home = hermes_core::get_hermes_home();
    let config_path = hermes_home.join("config.yaml");
    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap_or(serde_yaml::Value::Null);
            if let Some(platforms) = config.get("gateway").and_then(|g| g.get("platforms")).and_then(|p| p.as_sequence()) {
                let count = platforms.len();
                println!("  {} {count} platform(s) configured", green.apply_to("✓"));
                for platform in platforms {
                    if let Some(entry) = platform.as_mapping() {
                        if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                            let enabled = entry.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                            let status_str = if enabled {
                                green.apply_to("enabled").to_string()
                            } else {
                                dim.apply_to("disabled").to_string()
                            };
                            println!("    - {name}: {status_str}");
                        }
                    }
                }
            } else {
                println!("  {} No platforms configured in config.yaml", yellow.apply_to("→"));
                println!("    Set FEISHU_APP_ID/SECRET or WEIXIN_SESSION_KEY env vars,");
                println!("    or add platforms to ~/.hermes/config.yaml");
            }
        }
    }

    println!();

    // Show recent logs if available
    let log_dir = hermes_home.join("logs");
    if log_dir.exists() {
        let gateway_logs: Vec<_> = std::fs::read_dir(&log_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains("gateway"))
                    .unwrap_or(false)
            })
            .collect();

        if !gateway_logs.is_empty() {
            println!("  {} Log files:", dim.apply_to("→"));
            for entry in &gateway_logs {
                if let Ok(meta) = entry.metadata() {
                    let size = meta.len();
                    let size_str = if size > 1024 * 1024 {
                        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
                    } else if size > 1024 {
                        format!("{:.1} KB", size as f64 / 1024.0)
                    } else {
                        format!("{size} B")
                    };
                    println!("    {}  ({size_str})", entry.file_name().to_string_lossy());
                }
            }
            println!();
        }
    }

    Ok(())
}

/// Install gateway as system service.
pub fn cmd_gateway_install(_force: bool, _system: bool, _run_as_user: Option<&str>) -> Result<(), String> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    if has_systemd() {
        install_systemd_service()?;
        println!("  {} systemd unit installed", green.apply_to("✓"));
        if let Some(ref path) = systemd_unit_path() {
            println!("    Unit file: {}", path.display());
        }
        println!("    Enable with: systemctl --user enable {}", get_service_name());
    } else if has_launchctl() {
        install_launchd_service()?;
        println!("  {} launchd agent installed", green.apply_to("✓"));
        println!("    Plist: {}", launchd_plist_path().display());
    } else if is_windows() {
        println!("  {} Windows: use 'hermes gateway start' to run in background", yellow.apply_to("→"));
        println!("    For auto-start, add a shortcut to the Startup folder.");
    } else {
        return Err("No supported service manager found (systemd, launchd, or Windows)".to_string());
    }

    Ok(())
}

/// Uninstall gateway service.
pub fn cmd_gateway_uninstall(_system: bool) -> Result<(), String> {
    let green = Style::new().green();
    let yellow = Style::new().yellow();

    if has_systemd() {
        uninstall_systemd_service()?;
        println!("  {} systemd unit removed", green.apply_to("✓"));
    } else if has_launchctl() {
        uninstall_launchd_service()?;
        println!("  {} launchd agent removed", green.apply_to("✓"));
    } else if is_windows() {
        let _ = stop_windows_background();
        println!("  {} Background gateway stopped", green.apply_to("✓"));
    } else {
        // Kill manual background
        let pid_file = pid_file_path();
        if pid_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    if is_process_running(pid) {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(pid as i32, libc::SIGTERM);
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&pid_file);
        }
        println!("  {} Manual gateway stopped", green.apply_to("✓"));
    }

    // Clean up PID file and logs
    let pid_file = pid_file_path();
    if pid_file.exists() {
        let _ = std::fs::remove_file(&pid_file);
    }

    println!("  {} Cleaned up PID file", yellow.apply_to("→"));
    println!();
    println!("  {} Note: Sessions and config are preserved.", yellow.apply_to("ℹ"));

    Ok(())
}

/// Restart gateway service.
pub fn cmd_gateway_restart(_system: bool, _all: bool) -> Result<(), String> {
    cmd_gateway_stop(false, false)?;
    cmd_gateway_start(false, false)?;
    Ok(())
}

/// Configure messaging platforms interactively.
pub fn cmd_gateway_setup() -> Result<(), String> {
    let cyan = Style::new().cyan();

    println!();
    println!("{}", cyan.apply_to("◆ Gateway Platform Setup"));
    println!();
    println!("  Supported platforms:");
    println!("    - Telegram      (token-based bot)");
    println!("    - Discord       (bot token)");
    println!("    - Slack         (Bot token + signing secret)");
    println!("    - WhatsApp      (via Cloud API)");
    println!("    - Signal        (via signal-cli)");
    println!("    - Feishu/Lark   (app credentials)");
    println!("    - Line          (channel credentials)");
    println!("    - SMS           (via Twilio)");
    println!("    - Email         (via SMTP/IMAP)");
    println!();
    println!("  {}", Style::new().dim().apply_to("Run `hermes gateway start` to launch after configuration."));
    println!("  Platform credentials are stored in ~/.hermes/gateway_config.yaml");
    println!();

    Ok(())
}
