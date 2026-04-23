#![allow(dead_code)]
//! Shell completion generation command.

use clap::Command;
use clap_complete::{generate, Shell};
use std::io;

/// Build a command tree matching the real `hermez` binary.
fn build_hermez_command() -> Command {
    let mut cmd = Command::new("hermez")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Hermez Agent CLI")
        .subcommand(Command::new("chat").about("Interactive chat session").args([
            clap::Arg::new("model").short('m').long("model").value_name("MODEL"),
            clap::Arg::new("quiet").short('q').long("quiet").action(clap::ArgAction::SetTrue),
            clap::Arg::new("skip_context_files").long("skip-context-files").action(clap::ArgAction::SetTrue),
            clap::Arg::new("skip_memory").long("skip-memory").action(clap::ArgAction::SetTrue),
            clap::Arg::new("voice").long("voice").action(clap::ArgAction::SetTrue),
        ]))
        .subcommand(Command::new("setup").about("Interactive setup wizard").args([
            clap::Arg::new("section").value_name("SECTION"),
        ]))
        .subcommand(Command::new("tools").about("Manage tools").subcommands([
            Command::new("list").about("List all tools"),
            Command::new("info").about("Show tool details").args([clap::Arg::new("name").required(true)]),
        ]))
        .subcommand(Command::new("skills").about("Manage skills").subcommands([
            Command::new("list").about("List installed skills"),
            Command::new("info").about("Show skill details").args([clap::Arg::new("name").required(true)]),
            Command::new("enable").about("Enable a skill").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("platform").short('p').long("platform"),
            ]),
            Command::new("disable").about("Disable a skill").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("platform").short('p').long("platform"),
            ]),
            Command::new("commands").about("List skill slash commands"),
            Command::new("reset").about("Reset skills to factory defaults"),
        ]))
        .subcommand(Command::new("gateway").about("Run messaging gateway").subcommands([
            Command::new("run").about("Run gateway in foreground"),
            Command::new("start").about("Start gateway service"),
            Command::new("stop").about("Stop gateway service"),
            Command::new("status").about("Show gateway status"),
            Command::new("install").about("Install gateway service"),
            Command::new("uninstall").about("Uninstall gateway service"),
            Command::new("migrate-legacy").about("Migrate legacy gateway config to new format"),
        ]))
        .subcommand(Command::new("doctor").about("Diagnose configuration"))
        .subcommand(Command::new("models").about("List available models"))
        .subcommand(Command::new("profiles").about("Manage profiles").subcommands([
            Command::new("list").about("List all profiles"),
            Command::new("create").about("Create a profile").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("clone").long("clone").action(clap::ArgAction::SetTrue),
                clap::Arg::new("clone_all").long("clone-all").action(clap::ArgAction::SetTrue),
                clap::Arg::new("clone_from").long("clone-from"),
                clap::Arg::new("no_alias").long("no-alias").action(clap::ArgAction::SetTrue),
            ]),
            Command::new("use").about("Switch to profile").args([
                clap::Arg::new("name").required(true),
            ]),
            Command::new("delete").about("Delete a profile").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("force").short('f').long("force").action(clap::ArgAction::SetTrue),
                clap::Arg::new("yes").short('y').long("yes").action(clap::ArgAction::SetTrue),
            ]),
            Command::new("show").about("Show profile details").args([
                clap::Arg::new("name").required(true),
            ]),
            Command::new("alias").about("Manage profile aliases").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("target").value_name("TARGET"),
                clap::Arg::new("remove").long("remove").action(clap::ArgAction::SetTrue),
            ]),
            Command::new("rename").about("Rename a profile").args([
                clap::Arg::new("old_name").required(true),
                clap::Arg::new("new_name").long("new-name").required(true),
            ]),
            Command::new("export").about("Export profile to archive").args([
                clap::Arg::new("name").required(true),
                clap::Arg::new("output").short('o').long("output"),
            ]),
            Command::new("import").about("Import profile from archive").args([
                clap::Arg::new("path").required(true),
                clap::Arg::new("name").long("name"),
            ]),
        ]))
        .subcommand(Command::new("sessions").about("Manage sessions").subcommands([
            Command::new("list").about("List sessions").args([
                clap::Arg::new("limit").short('l').long("limit").default_value("20"),
                clap::Arg::new("source").short('s').long("source"),
            ]),
            Command::new("export").about("Export session").args([
                clap::Arg::new("session_id").required(true),
                clap::Arg::new("output").short('o').long("output"),
            ]),
            Command::new("delete").about("Delete session").args([clap::Arg::new("session_id").required(true)]),
            Command::new("prune").about("Delete old sessions"),
            Command::new("rename").about("Rename session").args([clap::Arg::new("session_id").required(true)]),
            Command::new("stats").about("Session statistics"),
            Command::new("browse").about("Interactive session picker"),
        ]))
        .subcommand(Command::new("config").about("Manage config").subcommands([
            Command::new("show").about("Show current config"),
            Command::new("edit").about("Edit config file"),
            Command::new("set").about("Set a config value"),
            Command::new("path").about("Print config file path"),
            Command::new("env-path").about("Print .env file path"),
            Command::new("check").about("Check for missing config"),
            Command::new("migrate").about("Migrate config options"),
        ]))
        .subcommand(Command::new("batch").about("Parallel batch processing").subcommands([
            Command::new("run").about("Run batch job"),
            Command::new("status").about("Show batch status"),
            Command::new("list").about("List batch jobs"),
        ]))
        .subcommand(Command::new("cron").about("Manage cron jobs").subcommands([
            Command::new("list").about("List cron jobs"),
            Command::new("create").about("Create a cron job"),
            Command::new("edit").about("Edit a cron job"),
            Command::new("pause").about("Pause a cron job"),
            Command::new("resume").about("Resume a cron job"),
            Command::new("run").about("Run a cron job"),
            Command::new("remove").about("Remove a cron job"),
            Command::new("status").about("Cron scheduler status"),
            Command::new("tick").about("Run due jobs once"),
        ]))
        .subcommand(Command::new("auth").about("Manage authentication").subcommands([
            Command::new("login").about("Login to a provider"),
            Command::new("status").about("Show auth status"),
            Command::new("add").about("Add a credential"),
            Command::new("list").about("List credentials"),
            Command::new("remove").about("Remove a credential"),
            Command::new("reset").about("Reset credential state"),
        ]))
        .subcommand(Command::new("logout").about("Clear stored credentials"))
        .subcommand(Command::new("status").about("Show component status"))
        .subcommand(Command::new("insights").about("Session analytics"))
        .subcommand(Command::new("backup").about("Backup state").args([
            clap::Arg::new("output").short('o').long("output"),
            clap::Arg::new("include_sessions").long("include-sessions").action(clap::ArgAction::SetTrue),
        ]))
        .subcommand(Command::new("restore").about("Restore from backup").args([
            clap::Arg::new("path").required(true),
            clap::Arg::new("force").short('f').long("force").action(clap::ArgAction::SetTrue),
        ]))
        .subcommand(Command::new("backup-list").about("List available backups"))
        .subcommand(Command::new("debug").about("Print debug info"))
        .subcommand(Command::new("debug-delete").about("Delete a debug paste").args([
            clap::Arg::new("url").required(true),
        ]))
        .subcommand(Command::new("dump").about("Dump session data").args([
            clap::Arg::new("session_id").value_name("SESSION_ID"),
        ]))
        .subcommand(Command::new("completion").about("Generate shell completion").args([
            clap::Arg::new("shell").short('s').long("shell").default_value("bash"),
        ]));

    // Global args
    cmd = cmd
        .arg(clap::Arg::new("verbose").short('v').long("verbose").global(true).action(clap::ArgAction::SetTrue))
        .arg(clap::Arg::new("hermez_home").long("hermez-home").global(true).value_name("DIR"));

    cmd
}

/// Generate shell completion script.
pub fn cmd_completion(shell: &str) -> anyhow::Result<()> {
    let shell = match shell {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "elvish" => Shell::Elvish,
        "powershell" => Shell::PowerShell,
        _ => {
            anyhow::bail!("Unsupported shell: {}. Supported: bash, zsh, fish, elvish, powershell", shell);
        }
    };

    let mut cmd = build_hermez_command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut io::stdout());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_command_has_all_subcommands() {
        let cmd = build_hermez_command();
        let subcommand_names: Vec<&str> = cmd.get_subcommands()
            .map(|s| s.get_name())
            .collect();
        // Verify key subcommands are present
        assert!(subcommand_names.contains(&"chat"));
        assert!(subcommand_names.contains(&"setup"));
        assert!(subcommand_names.contains(&"gateway"));
        assert!(subcommand_names.contains(&"status"));
        assert!(subcommand_names.contains(&"insights"));
        assert!(subcommand_names.contains(&"backup"));
    }

    #[test]
    fn test_unsupported_shell_returns_error() {
        let result = cmd_completion("tcsh");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported shell"));
    }

    #[test]
    fn test_completion_bash_succeeds() {
        use std::io::Cursor;
        // Re-build command to capture output (cmd_completion writes to stdout)
        let mut cmd = build_hermez_command();
        let name = cmd.get_name().to_string();
        let mut buf = Cursor::new(Vec::new());
        generate(Shell::Bash, &mut cmd, name, &mut buf);
        let output = String::from_utf8(buf.into_inner()).unwrap();
        assert!(output.contains("hermez"));
    }
}
