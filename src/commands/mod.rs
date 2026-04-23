//! Hermez CLI command definitions and dispatch.
//!
//! This module contains all clap argument schemas and the command dispatch logic.
//! Extracted from the original 1,900-line `main.rs`.

pub mod dispatch;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hermez", about = "Hermez Agent CLI", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Enable verbose (debug) logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Hermez home directory override (profiles)
    #[arg(long, global = true)]
    pub hermez_home: Option<String>,

    /// Profile name (resolves HERMEZ_HOME before subcommands)
    #[arg(short = 'p', long, global = true)]
    pub profile: Option<String>,
}
#[derive(Subcommand)]
pub enum Commands {
    /// Interactive chat session with the agent
    Chat {
        /// Model to use
        #[arg(short, long)]
        model: Option<String>,
        /// Single query (non-interactive mode)
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// Optional local image path to attach to a single query
        #[arg(long)]
        image: Option<String>,
        /// Comma-separated toolsets to enable
        #[arg(short = 't', long)]
        toolsets: Option<String>,
        /// Preload one or more skills (comma-separated)
        #[arg(short = 's', long)]
        skills: Option<String>,
        /// Inference provider selection (default: auto)
        #[arg(long)]
        provider: Option<String>,
        /// Resume a previous session by ID
        #[arg(short = 'r', long)]
        resume: Option<String>,
        /// Resume last session (or by name if provided)
        #[arg(short = 'c', long)]
        continue_last: Option<Option<String>>,
        /// Run in an isolated git worktree
        #[arg(short = 'w', long)]
        worktree: bool,
        /// Enable filesystem checkpoints before destructive file operations
        #[arg(long)]
        checkpoints: bool,
        /// Maximum tool-calling iterations per turn
        #[arg(long)]
        max_turns: Option<u32>,
        /// Bypass all dangerous command approval prompts
        #[arg(long)]
        yolo: bool,
        /// Include session ID in system prompt
        #[arg(long)]
        pass_session_id: bool,
        /// Session source tag (default: cli)
        #[arg(long)]
        source: Option<String>,
        /// Quiet mode (suppress debug output)
        #[arg(long)]
        quiet: bool,
        /// Verbose output (show tool previews, debug info)
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Skip loading context files
        #[arg(long)]
        skip_context_files: bool,
        /// Skip memory loading
        #[arg(long)]
        skip_memory: bool,
        /// Enable voice mode
        #[arg(long)]
        voice: bool,
    },
    /// Interactive setup wizard
    Setup {
        /// Section to configure (model, terminal, agent, gateway, tools, tts)
        section: Option<String>,
        /// Non-interactive mode (use defaults/env vars)
        #[arg(long)]
        non_interactive: bool,
        /// Reset configuration to defaults
        #[arg(long)]
        reset: bool,
    },
    /// Backup Hermez state
    Backup {
        /// Output directory (default: current dir)
        #[arg(short, long)]
        output: Option<String>,
        /// Include session database
        #[arg(long)]
        include_sessions: bool,
        /// Quick snapshot (critical state only)
        #[arg(short, long)]
        quick: bool,
        /// Snapshot label
        #[arg(short, long)]
        label: Option<String>,
    },
    /// Restore from a backup
    Restore {
        /// Backup directory path
        path: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// List available backups
    BackupList,
    /// Print debug info
    Debug,
    /// Generate and share debug report
    DebugShare {
        /// Number of log lines to include
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
        /// Expiration in days
        #[arg(long, default_value_t = 7)]
        expire_days: usize,
        /// Print locally only (don't upload)
        #[arg(long)]
        local_only: bool,
    },
    /// Delete a previously uploaded debug paste
    DebugDelete {
        /// URL of the debug paste to delete
        url: String,
    },
    /// Dump session data for debugging
    Dump {
        /// Session ID or prefix
        session_id: Option<String>,
        /// Show redacted API key prefixes
        #[arg(long)]
        show_keys: bool,
    },
    /// Manage tool configurations
    Tools {
        #[command(subcommand)]
        action: Option<ToolAction>,
    },
    /// Manage skill configurations
    Skills {
        #[command(subcommand)]
        action: Option<SkillAction>,
    },
    /// Run the messaging gateway
    Gateway {
        #[command(subcommand)]
        action: Option<GatewayAction>,
    },
    /// Diagnose common configuration issues
    Doctor {
        /// Auto-fix detected issues
        #[arg(long)]
        fix: bool,
    },
    /// List available models
    Models,
    /// Manage profiles
    Profiles {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },
    /// Manage conversation sessions
    Sessions {
        #[command(subcommand)]
        action: Option<SessionAction>,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Parallel batch processing on JSONL datasets
    Batch {
        #[command(subcommand)]
        action: Option<BatchAction>,
    },
    /// SWE (Software Engineering) evaluation
    Swe {
        #[command(subcommand)]
        action: Option<SweAction>,
    },
    /// Manage scheduled cron jobs
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
    },
    /// Manage authentication
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Manage CLI skins/themes
    Skin {
        #[command(subcommand)]
        action: Option<SkinAction>,
    },
    /// Show status of all components
    Status {
        /// Show all redacted details
        #[arg(long)]
        all: bool,
        /// Run deep checks (slower)
        #[arg(long)]
        deep: bool,
    },
    /// Show session analytics and insights
    Insights {
        /// Number of days to analyze (default: 30)
        #[arg(long, default_value_t = 30)]
        days: usize,
        /// Filter by platform/source
        #[arg(long)]
        source: Option<String>,
    },
    /// Generate shell completion script
    Completion {
        /// Shell type: bash, zsh, fish, elvish, powershell
        #[arg(short, long, default_value = "bash")]
        shell: String,
    },
    /// Show version information
    Version,
    /// View and filter log files
    Logs {
        /// Log to view: agent (default), errors, gateway, or 'list'
        log_name: Option<String>,
        /// Number of lines to show
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Follow log in real time
        #[arg(short, long)]
        follow: bool,
        /// Minimum log level
        #[arg(long)]
        level: Option<String>,
        /// Filter by session ID
        #[arg(long)]
        session: Option<String>,
        /// Filter by component
        #[arg(long)]
        component: Option<String>,
        /// Show lines since time ago (e.g. 1h, 30m)
        #[arg(long)]
        since: Option<String>,
    },
    /// Manage webhook subscriptions
    Webhook {
        #[command(subcommand)]
        action: WebhookAction,
    },
    /// Manage plugins
    Plugins {
        #[command(subcommand)]
        action: Option<PluginAction>,
    },
    /// Configure external memory provider
    Memory {
        #[command(subcommand)]
        action: Option<MemoryAction>,
    },
    /// Log out and clear stored credentials
    Logout {
        /// Provider to log out from (default: all)
        #[arg(long)]
        provider: Option<String>,
    },
    /// Restore a backup from a zip archive
    Import {
        /// Backup archive path (.zip)
        path: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Manage MCP server connections
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// Interactive model selection and management
    Model {
        #[command(subcommand)]
        action: Option<ModelAction>,
    },
    /// OAuth login for supported providers
    Login {
        /// Provider name (google, anthropic, openai)
        provider: String,
        /// OAuth client ID
        #[arg(long)]
        client_id: Option<String>,
        /// Skip browser auto-open
        #[arg(long)]
        no_browser: bool,
        /// OAuth scopes
        #[arg(long)]
        scopes: Option<String>,
        /// Nous portal base URL
        #[arg(long)]
        portal_url: Option<String>,
        /// Nous inference base URL
        #[arg(long)]
        inference_url: Option<String>,
        /// Request timeout in seconds
        #[arg(long)]
        timeout: Option<f64>,
        /// Custom CA bundle path
        #[arg(long)]
        ca_bundle: Option<String>,
        /// Disable TLS verification
        #[arg(long)]
        insecure: bool,
    },
    /// Manage device pairings
    Pairing {
        #[command(subcommand)]
        action: PairingAction,
    },
    /// Self-update Hermez Agent
    Update {
        /// Use preview (pre-release) channel
        #[arg(long)]
        preview: bool,
        /// Force upgrade even when up to date
        #[arg(long)]
        force: bool,
        /// Gateway mode: use file-based IPC (internal use)
        #[arg(long)]
        gateway: bool,
    },
    /// Uninstall Hermez Agent
    Uninstall {
        /// Preserve data directory
        #[arg(long)]
        keep_data: bool,
        /// Preserve config
        #[arg(long)]
        keep_config: bool,
        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Interactive analytics dashboard
    Dashboard {
        /// Port to listen on
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Host to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Don't auto-open browser
        #[arg(long)]
        no_open: bool,
        /// Disable HTTPS redirect (testing)
        #[arg(long)]
        insecure: bool,
        /// Serve web dashboard over HTTP instead of TUI
        #[arg(long)]
        serve: bool,
    },
    /// Configure WhatsApp Cloud API
    WhatsApp {
        /// Action: setup, connect, status
        action: String,
        /// Access token
        #[arg(long)]
        token: Option<String>,
        /// Phone Number ID
        #[arg(long)]
        phone_id: Option<String>,
    },
    /// Agent Client Protocol (IDE integration)
    Acp {
        /// Action: status, install, run
        action: Option<String>,
        /// Editor name (vscode, zed, jetbrains)
        #[arg(long)]
        editor: Option<String>,
    },
    /// Migrate from another agent system
    Claw {
        /// Action: migrate, cleanup
        action: String,
        /// Source system path or name (claude-code, chatgpt, or ~/.openclaw)
        #[arg(long)]
        source: Option<String>,
        /// Force migration
        #[arg(long)]
        force: bool,
        /// Preview only — stop after showing what would be migrated
        #[arg(long)]
        dry_run: bool,
        /// Migration preset: user-data (excludes secrets) or full
        #[arg(long, default_value = "full")]
        preset: String,
        /// Overwrite existing files (default: skip conflicts)
        #[arg(long)]
        overwrite: bool,
        /// Include allowlisted secrets
        #[arg(long)]
        migrate_secrets: bool,
        /// Skip confirmation prompts
        #[arg(short = 'y', long)]
        yes: bool,
        /// Path to copy workspace instructions into
        #[arg(long)]
        workspace_target: Option<String>,
        /// How to handle skill conflicts (choices: skip, overwrite, rename)
        #[arg(long, default_value = "skip")]
        skill_conflict: String,
    },
}

/// Subcommands for model management.
#[derive(Subcommand)]
pub(crate) enum ModelAction {
    /// Interactive model selection
    Browse,
    /// List available models
    #[command(alias = "ls")]
    List,
    /// Switch to a different model
    Switch {
        /// Model identifier (e.g., anthropic/claude-sonnet-4-6)
        model: String,
    },
    /// Show model details
    Info {
        /// Model identifier
        model: String,
    },
}

/// Subcommands for skin management.
#[derive(Subcommand)]
pub(crate) enum SkinAction {
    /// List available skins
    #[command(alias = "ls")]
    List,
    /// Apply a skin
    Apply {
        /// Skin name
        name: String,
    },
    /// Preview a skin without applying
    Preview {
        /// Skin name
        name: String,
    },
}

/// Subcommands for device pairing.
#[derive(Subcommand)]
pub(crate) enum PairingAction {
    /// Show pending + approved pairings
    #[command(alias = "ls")]
    List,
    /// Approve a pairing code
    Approve {
        /// Platform name (telegram, discord, slack, whatsapp)
        platform: String,
        /// Pairing code
        code: String,
    },
    /// Revoke user access
    Revoke {
        /// Platform name
        platform: String,
        /// Pairing code or user ID to revoke
        code: String,
    },
    /// Clear all pending codes
    ClearPending,
}

#[derive(Subcommand)]
pub(crate) enum WebhookAction {
    /// Create a webhook subscription
    #[command(alias = "add")]
    Subscribe {
        /// Route name
        name: String,
        /// Prompt template with {dot.notation} payload refs
        #[arg(long, default_value = "")]
        prompt: String,
        /// Comma-separated event types
        #[arg(long, default_value = "")]
        events: String,
        /// Description
        #[arg(long, default_value = "")]
        description: String,
        /// Delivery target
        #[arg(long, default_value = "log")]
        deliver: String,
        /// Target chat ID for cross-platform delivery
        #[arg(long)]
        deliver_chat_id: Option<String>,
        /// Comma-separated skill names
        #[arg(long, default_value = "")]
        skills: String,
        /// HMAC secret for payload verification
        #[arg(long)]
        secret: Option<String>,
    },
    /// List webhook subscriptions
    #[command(alias = "ls")]
    List,
    /// Remove a subscription
    #[command(alias = "rm")]
    Remove {
        /// Subscription name
        name: String,
    },
    /// Send a test POST to a webhook route
    Test {
        /// Subscription name
        name: String,
        /// JSON payload to send
        #[arg(long, default_value = "")]
        payload: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum PluginAction {
    /// Install a plugin from Git
    Install {
        /// Git URL or owner/repo shorthand
        identifier: String,
        /// Remove existing and reinstall
        #[arg(short, long)]
        force: bool,
    },
    /// Update a plugin
    Update {
        /// Plugin name
        name: String,
    },
    /// Remove a plugin
    #[command(alias = "rm", alias = "uninstall")]
    Remove {
        /// Plugin name
        name: String,
    },
    /// List installed plugins
    #[command(alias = "ls")]
    List,
    /// Enable a disabled plugin
    Enable {
        /// Plugin name
        name: String,
    },
    /// Disable a plugin
    Disable {
        /// Plugin name
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum MemoryAction {
    /// Interactive provider selection and configuration
    Setup,
    /// Show current memory provider config
    Status,
    /// Disable external provider (built-in only)
    Off,
}

#[derive(Subcommand)]
pub(crate) enum McpAction {
    /// List configured MCP servers
    #[command(alias = "ls")]
    List,
    /// Add an MCP server
    Add {
        /// Server name
        name: String,
        /// HTTP/SSE endpoint URL
        #[arg(long)]
        url: Option<String>,
        /// Command to run
        #[arg(long)]
        command: Option<String>,
        /// Command arguments
        #[arg(long, default_values_t = Vec::<String>::new())]
        args: Vec<String>,
        /// Auth method (oauth, header)
        #[arg(long)]
        auth: Option<String>,
        /// Known MCP preset name
        #[arg(long)]
        preset: Option<String>,
        /// Environment variables (KEY=VALUE)
        #[arg(long, default_values_t = Vec::<String>::new())]
        env: Vec<String>,
    },
    /// Remove an MCP server
    #[command(alias = "rm", alias = "delete")]
    Remove {
        /// Server name
        name: String,
    },
    /// Test connection to an MCP server
    Test {
        /// Server name
        name: String,
    },
    /// Interactive MCP configuration
    #[command(alias = "config")]
    Configure {
        /// Server name
        name: String,
    },
    /// Run as MCP stdio server
    Serve {
        /// Enable verbose logging on stderr
        #[arg(short = 'v', long)]
        verbose: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ToolAction {
    /// List all available toolsets with enabled/disabled status
    #[command(alias = "ls")]
    List {
        /// Platform to show (default: cli)
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Show tool/toolset details
    Info { name: String },
    /// Disable one or more toolsets
    Disable {
        /// Tool names to disable
        names: Vec<String>,
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Enable one or more toolsets
    Enable {
        /// Tool names to enable
        names: Vec<String>,
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Disable all toolsets
    #[command(alias = "disable-all")]
    DisableAll {
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Enable all toolsets
    #[command(alias = "enable-all")]
    EnableAll {
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Batch disable multiple toolsets
    #[command(alias = "disable-batch")]
    DisableBatch {
        /// Tool names to disable
        names: Vec<String>,
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Batch enable multiple toolsets
    #[command(alias = "enable-batch")]
    EnableBatch {
        /// Tool names to enable
        names: Vec<String>,
        /// Platform to apply to
        #[arg(long, default_value = "cli")]
        platform: String,
    },
    /// Show summary of enabled tools per platform
    Summary,
}

#[derive(Subcommand)]
pub(crate) enum SkillAction {
    /// List installed skills
    #[command(alias = "ls")]
    List {
        /// Filter by source: all, hub, builtin, local
        #[arg(long, default_value = "all")]
        source: String,
    },
    /// Search skill registries
    Search {
        /// Search query
        query: String,
        /// Filter by source
        #[arg(long, default_value = "all")]
        source: String,
        /// Max results
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Browse all available skills (paginated)
    Browse {
        /// Page number
        #[arg(long, default_value_t = 1)]
        page: usize,
        /// Results per page
        #[arg(long, default_value_t = 20)]
        size: usize,
        /// Filter by source
        #[arg(long, default_value = "all")]
        source: String,
    },
    /// Install a skill
    Install {
        /// Skill identifier
        identifier: String,
        /// Category folder to install into
        #[arg(long, default_value = "")]
        category: String,
        /// Force install despite existing
        #[arg(long)]
        force: bool,
        /// Skip confirmation prompts
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Preview a skill without installing
    Inspect {
        /// Skill identifier
        identifier: String,
    },
    /// Show skill details
    Info { name: String },
    /// Enable a disabled skill
    Enable {
        /// Skill name
        name: String,
        /// Platform (e.g., cli, telegram, discord)
        #[arg(short, long)]
        platform: Option<String>,
    },
    /// Disable a skill
    Disable {
        /// Skill name
        name: String,
        /// Platform (e.g., cli, telegram, discord)
        #[arg(short, long)]
        platform: Option<String>,
    },
    /// Uninstall a skill
    Uninstall {
        /// Skill name to remove
        name: String,
    },
    /// Check installed skills for updates
    Check {
        /// Specific skill to check (default: all)
        name: Option<String>,
    },
    /// Update installed hub skills
    Update {
        /// Specific skill to update (default: all)
        name: Option<String>,
    },
    /// Re-scan installed hub skills
    Audit {
        /// Specific skill to audit (default: all)
        name: Option<String>,
    },
    /// List discovered skill slash commands
    Commands,
    /// Publish a skill to a registry
    Publish {
        /// Skill name
        name: String,
        /// Registry URL
        #[arg(long)]
        registry: Option<String>,
        /// Target GitHub repo (owner/repo)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Export/import skill configurations
    Snapshot {
        #[command(subcommand)]
        snapshot_action: Option<SnapshotAction>,
    },
    /// Manage skill sources (taps)
    Tap {
        #[command(subcommand)]
        tap_action: Option<TapAction>,
    },
    /// Interactive skill configuration
    Config,
    /// Reset skills to factory defaults
    Reset,
}

/// Subcommands for skill snapshots.
#[derive(Subcommand)]
pub(crate) enum SnapshotAction {
    /// Export installed skills to a file
    Export {
        /// Output file path
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Import and install skills from a file
    Import {
        /// Input file path
        path: String,
        /// Force import despite existing
        #[arg(long)]
        force: bool,
    },
}

/// Subcommands for skill taps.
#[derive(Subcommand)]
pub(crate) enum TapAction {
    /// List configured taps
    #[command(alias = "ls")]
    List,
    /// Add a GitHub repo as skill source
    Add {
        /// GitHub repo URL or owner/repo
        repo: String,
    },
    /// Remove a tap
    #[command(alias = "rm")]
    Remove {
        /// Tap name
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileAction {
    /// List all profiles
    #[command(alias = "ls")]
    List,
    /// Create a new profile
    #[command(alias = "add")]
    Create {
        name: String,
        /// Copy config.yaml, .env, SOUL.md from active profile
        #[arg(long)]
        clone: bool,
        /// Full copy of active profile
        #[arg(long)]
        clone_all: bool,
        /// Source profile to clone from
        #[arg(long)]
        clone_from: Option<String>,
        /// Skip wrapper script creation
        #[arg(long)]
        no_alias: bool,
    },
    /// Switch to a profile
    Use { name: String },
    /// Delete a profile
    #[command(alias = "rm")]
    Delete {
        /// Profile name
        name: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
        /// Skip confirmation (alias)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show profile details
    Show {
        /// Profile name
        name: String,
    },
    /// Manage wrapper scripts
    Alias {
        /// Profile name
        name: String,
        /// Remove the wrapper script
        #[arg(long)]
        remove: bool,
        /// Custom alias name
        #[arg(long)]
        alias_name: Option<String>,
    },
    /// Rename a profile
    Rename {
        /// Current name
        old_name: String,
        /// New name
        #[arg(long)]
        new_name: String,
    },
    /// Export a profile to archive
    Export {
        /// Profile name
        name: String,
        /// Output file path
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Import a profile from archive
    Import {
        /// Archive file path
        path: String,
        /// Profile name (default: inferred from archive)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum SessionAction {
    /// List recent sessions
    #[command(alias = "ls")]
    List {
        /// Maximum number of sessions to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
        /// Filter by source (e.g., cli, telegram)
        #[arg(short, long)]
        source: Option<String>,
    },
    /// Delete a session
    #[command(alias = "rm")]
    Delete {
        /// Session ID or prefix
        session_id: String,
        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Search sessions by query
    Search {
        /// Search query
        query: String,
        /// Maximum number of results
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Show session statistics
    Stats {
        /// Filter by source
        #[arg(short, long)]
        source: Option<String>,
    },
    /// Rename a session's title
    Rename {
        /// Session ID
        session_id: String,
        /// New title
        #[arg(short, long)]
        title: String,
    },
    /// Prune old sessions
    Prune {
        /// Delete sessions older than this many days (default: 90)
        #[arg(long, default_value_t = 90)]
        older_than: i64,
        /// Filter by source
        #[arg(short, long)]
        source: Option<String>,
        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Interactive session browser
    Browse {
        /// Filter by source
        #[arg(short, long)]
        source: Option<String>,
        /// Maximum number of sessions to show
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Export sessions to JSONL
    Export {
        /// Output file path (use - for stdout)
        path: String,
        /// Filter by source
        #[arg(short, long)]
        source: Option<String>,
        /// Export a specific session by ID
        #[arg(long)]
        session_id: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Show current configuration
    Show {
        /// Show full YAML config
        #[arg(long)]
        verbose: bool,
    },
    /// Edit configuration file
    Edit,
    /// Set a configuration value
    Set {
        /// Config key (supports dotted paths, e.g., agent.model)
        key: String,
        /// Value to set
        value: String,
    },
    /// Print config file path
    Path,
    /// Print .env file path
    EnvPath,
    /// Check for missing/outdated config
    Check,
    /// Update config with new options
    Migrate,
}

#[derive(Subcommand)]
pub(crate) enum BatchAction {
    /// Run batch processing on a JSONL dataset
    Run {
        /// Path to the JSONL dataset file
        dataset: String,
        /// Run name (used for output directory and checkpoint)
        #[arg(short, long)]
        name: Option<String>,
        /// Model to use
        #[arg(short, long)]
        model: Option<String>,
        /// Number of prompts per batch
        #[arg(long, default_value_t = 10)]
        batch_size: usize,
        /// Number of parallel workers
        #[arg(long, default_value_t = 4)]
        workers: usize,
        /// Max tool-calling iterations per prompt
        #[arg(long, default_value_t = 90)]
        max_iterations: usize,
        /// Truncate dataset to N samples (0 = all)
        #[arg(long, default_value_t = 0)]
        max_samples: usize,
        /// Resume from checkpoint
        #[arg(long)]
        resume: bool,
        /// Toolset distribution for sampling
        #[arg(long)]
        distribution: Option<String>,
    },
    /// List available toolset distributions
    Distributions,
    /// Show batch run status
    Status {
        /// Run name
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum SweAction {
    /// Run SWE evaluation on a dataset
    Evaluate {
        /// Dataset source: path, hf://<name>, or "builtin"
        #[arg(default_value = "builtin")]
        dataset: String,
        /// Dataset split
        #[arg(short = 't', long, default_value = "test")]
        split: String,
        /// Sandbox backend: local or docker
        #[arg(short = 'b', long, default_value = "local")]
        sandbox: String,
        /// Max samples to evaluate
        #[arg(short = 'n', long, default_value_t = 0)]
        max_samples: usize,
        /// Output directory for report
        #[arg(short = 'o', long)]
        output: Option<String>,
        /// Model name (for agent + report metadata)
        #[arg(long)]
        model: Option<String>,
        /// Quick mode: fewer samples, faster
        #[arg(long)]
        quick: bool,
        /// Use a real AIAgent instead of placeholder evaluation
        #[arg(long)]
        agent: bool,
    },
    /// Run built-in benchmark suite
    Benchmark {
        /// Quick mode
        #[arg(long)]
        quick: bool,
    },
    /// Show environment info and check dependencies
    Env,
}

#[derive(Subcommand)]
pub(crate) enum GatewayAction {
    /// Run gateway in foreground
    Run {
        /// Enable verbose logging
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Suppress non-error output
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Replace running gateway instance
        #[arg(long)]
        replace: bool,
    },
    /// Start gateway as background service
    Start {
        /// Start on all platforms
        #[arg(long)]
        all: bool,
        /// Target Linux system-level gateway service (systemd)
        #[arg(long)]
        system: bool,
    },
    /// Stop gateway service
    Stop {
        /// Stop on all platforms
        #[arg(long)]
        all: bool,
        /// Target Linux system-level gateway service (systemd)
        #[arg(long)]
        system: bool,
    },
    /// Restart gateway service
    Restart {
        /// Restart system service (systemd/launchd)
        #[arg(long)]
        system: bool,
        /// Restart on all platforms
        #[arg(long)]
        all: bool,
    },
    /// Show gateway status
    Status {
        /// Run deep checks (slower)
        #[arg(long)]
        deep: bool,
        /// Target Linux system-level gateway service (systemd)
        #[arg(long)]
        system: bool,
    },
    /// Install gateway as systemd/launchd service
    Install {
        /// Force reinstall
        #[arg(long)]
        force: bool,
        /// Install as Linux system-level service (systemd)
        #[arg(long)]
        system: bool,
        /// User account the Linux system service should run as
        #[arg(long)]
        run_as_user: Option<String>,
    },
    /// Uninstall gateway service
    Uninstall {
        /// Target Linux system-level gateway service (systemd)
        #[arg(long)]
        system: bool,
    },
    /// Configure messaging platforms
    Setup,
    /// Migrate legacy gateway config to new format
    MigrateLegacy,
}

#[derive(Subcommand)]
pub(crate) enum CronAction {
    /// List scheduled jobs
    #[command(alias = "ls")]
    List {
        /// Include disabled jobs
        #[arg(long)]
        all: bool,
    },
    /// Create a new scheduled job
    #[command(alias = "add")]
    Create {
        /// Job name
        name: String,
        /// Cron expression or interval (e.g., "0 9 * * *" or "1h")
        #[arg(short, long)]
        schedule: String,
        /// Command or URL to execute
        #[arg(short, long)]
        command: String,
        /// Task instruction / prompt for the agent
        #[arg(long)]
        prompt: Option<String>,
        /// Delivery platform (e.g., telegram, discord, webhook)
        #[arg(long, default_value = "local")]
        delivery: Option<String>,
        /// Start disabled
        #[arg(long)]
        paused: bool,
        /// Repeat count (0 = infinite)
        #[arg(long, default_value_t = 0)]
        repeat: usize,
        /// Skill name to invoke
        #[arg(long)]
        skill: Option<String>,
        /// Script content to execute
        #[arg(long)]
        script: Option<String>,
    },
    /// Delete a scheduled job
    #[command(alias = "rm", alias = "delete")]
    Delete {
        /// Job ID
        job_id: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Pause a scheduled job
    Pause {
        /// Job ID
        job_id: String,
    },
    /// Resume a paused job
    Resume {
        /// Job ID
        job_id: String,
    },
    /// Edit a scheduled job
    Edit {
        /// Job ID
        job_id: String,
        /// New schedule
        #[arg(short, long)]
        schedule: Option<String>,
        /// New name
        #[arg(short, long)]
        name: Option<String>,
        /// New prompt
        #[arg(short, long)]
        prompt: Option<String>,
        /// New delivery target
        #[arg(long)]
        deliver: Option<String>,
        /// Repeat count
        #[arg(long)]
        repeat: Option<usize>,
        /// Script content
        #[arg(long)]
        script: Option<String>,
        /// Skill name to set
        #[arg(long)]
        skill: Option<String>,
        /// Add a skill to the job
        #[arg(long)]
        add_skill: Option<String>,
        /// Remove a skill from the job
        #[arg(long)]
        remove_skill: Option<String>,
        /// Clear all skills
        #[arg(long)]
        clear_skills: bool,
    },
    /// Trigger a job to run on next tick
    Run {
        /// Job ID
        job_id: String,
    },
    /// Show scheduler status
    Status,
    /// Run all due jobs once (debug)
    Tick,
}

#[derive(Subcommand)]
pub(crate) enum AuthAction {
    /// Add a pooled credential
    Add {
        /// Provider name (e.g., openai, anthropic)
        provider: String,
        /// Credential type
        #[arg(long, default_value = "api-key")]
        auth_type: String,
        /// API key value
        #[arg(long, alias = "key")]
        api_key: Option<String>,
        /// Label for this credential
        #[arg(long)]
        label: Option<String>,
        /// OAuth client id
        #[arg(long)]
        client_id: Option<String>,
        /// Skip browser auto-open for OAuth
        #[arg(long)]
        no_browser: bool,
        /// Nous portal base URL
        #[arg(long)]
        portal_url: Option<String>,
        /// Nous inference base URL
        #[arg(long)]
        inference_url: Option<String>,
        /// OAuth scope override
        #[arg(long)]
        scope: Option<String>,
        /// OAuth/network timeout in seconds
        #[arg(long)]
        timeout: Option<f64>,
        /// Disable TLS verification for OAuth login
        #[arg(long)]
        insecure: bool,
        /// Custom CA bundle for OAuth login
        #[arg(long)]
        ca_bundle: Option<String>,
    },
    /// List pooled credentials
    List {
        /// Filter by provider
        provider: Option<String>,
    },
    /// Remove a pooled credential
    Remove {
        /// Provider name
        provider: String,
        /// Credential index, entry id, or label
        target: String,
    },
    /// Reset exhaustion for a provider
    Reset {
        /// Provider name
        provider: String,
    },
    /// Show auth status
    Status,
}
