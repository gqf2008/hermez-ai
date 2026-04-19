# AGENTS.md — Hermes Agent (Rust)

> This file contains project-specific context for AI coding agents. Read it before making changes.

## Project Overview

Hermes Agent CLI (`hermes-rs`) is a self-evolving AI agent system built by Nous Research. It is a Rust rewrite of an earlier Python implementation, organized as a Cargo workspace with 12 crates and 3 binary targets.

- **Repository:** `https://github.com/NousResearch/hermes-agent`
- **Language:** Rust (minimum version 1.84, edition 2021)
- **License:** MIT
- **Version:** 0.1.0

The CLI supports interactive chat, 31+ subcommands, gateway integration with 19 messaging platforms, scheduled cron jobs, batch processing, skill management, and IDE integration via a JSON-RPC ACP server.

---

## Technology Stack

| Layer | Technology |
|-------|------------|
| Language | Rust 1.84+ |
| Async runtime | Tokio (full features) |
| CLI framework | clap v4 with derive macros |
| TUI / prompts | reedline, dialoguer, indicatif, crossterm |
| HTTP client | reqwest, async-openai |
| LLM providers | Anthropic, OpenAI, OpenRouter, Gemini, Codex, Kimi, Minimax, Z.ai, Nous, Bedrock, Custom |
| Database | SQLite with bundled rusqlite, WAL mode, FTS5 full-text search |
| Serialization | serde, serde_json, serde_yaml |
| Logging | tracing + tracing-subscriber (env-filter, json, time) |
| Prompt templates | minijinja |
| Cron parsing | cron crate |
| Docker API | bollard |
| SSH | russh |
| MCP protocol | rmcp |
| Testing | rstest, mockito, proptest, tokio-test, serial_test |

---

## Build and Test Commands

```bash
# Build all workspace crates (default members: 8 core crates)
cargo build --release

# Build with all features
cargo build --release --workspace --all-features

# Run all tests across the workspace
cargo test --workspace

# Run E2E CLI test suite (no API keys required)
bash scripts/e2e_test.sh          # release build
bash scripts/e2e_test.sh --debug  # debug build

# Run a specific crate's tests
cargo test -p hermes-llm
cargo test -p hermes-agent-engine

# Check formatting and lints (standard Rust)
cargo fmt --check
cargo clippy --workspace --all-targets
```

### Binary Outputs

After `cargo build --release`, three binaries are produced in `target/release/`:

- `hermes` — Main CLI (31+ subcommands)
- `hermes-agent` — Standalone conversation loop (stdin → stdout)
- `hermes-acp` — JSON-RPC IDE server (stdin/stdout)

---

## Architecture

The project follows a strict 5-tier layered architecture. Dependencies flow downward only (DAG).

```
Tier 5 — Binary Targets
  hermes (CLI) | hermes-agent (standalone) | hermes-acp (IDE server)

Tier 4 — CLI / Adapter Layer
  hermes-cli (31 subcommands, TUI, config, backup)
  hermes-gateway (19 platform adapters)
  hermes-cron (scheduler, job mgmt)
  hermes-batch (JSONL batch processing)
  hermes-compress (4-stage context compression)

Tier 3 — Agent Engine Layer
  hermes-agent-engine
    AIAgent::run_conversation(), tool dispatch, failover chain,
    memory manager, subagent delegation (depth ≤ 2, max 3 concurrent),
    smart model routing, title generator, trajectory saver,
    self-evolution, review agent, skill commands, budget tracking

Tier 2 — Service Layer
  hermes-tools (~60 tool modules, registry, toolsets, env backends)
  hermes-prompt (system prompt builder, context compressor, cache control)

Tier 1 — Infrastructure Layer
  hermes-llm (11 provider types, credential pool, retry, rate limit, token estimate)
  hermes-state (SQLite session DB, WAL, FTS5, insights engine)

Tier 0 — Core Layer
  hermes-core (HermesConfig, HermesError, constants, logging, home path)
```

### Crate Dependency Graph (simplified)

```
hermes (root) → hermes-cli, hermes-agent-engine, hermes-acp
hermes-cli → hermes-agent-engine, hermes-batch, hermes-cron, hermes-gateway
hermes-agent-engine → hermes-llm, hermes-tools, hermes-prompt, hermes-state
hermes-tools → hermes-llm, hermes-state
hermes-llm → hermes-core
hermes-prompt → hermes-llm
hermes-state → hermes-core
hermes-gateway, hermes-cron, hermes-compress, hermes-batch, hermes-rl → hermes-core
```

---

## Code Organization

### Workspace Members (12 crates)

| Crate | Responsibility | Default? |
|-------|----------------|----------|
| `hermes-core` | Config, errors, constants, home path, logging, platforms, redaction | Yes |
| `hermes-state` | SQLite session DB, schema, models, insights, FTS5 search | Yes |
| `hermes-llm` | LLM client, 11 providers, credential pool, retry, reasoning extraction, tool call parsing | Yes |
| `hermes-tools` | Tool registry, ~60 tool impls, toolsets, env backends (local/docker/ssh/daytona/singularity/modal) | Yes |
| `hermes-prompt` | System prompt builder, context compressor (4-stage), Anthropic cache control, injection scan, soul.md loader | Yes |
| `hermes-agent-engine` | AIAgent core loop, failover chain, memory manager, subagent, smart routing, trajectories, title gen | Yes |
| `hermes-cli` | 31 subcommand handlers, TUI, setup wizard, OAuth, backup, gateway mgmt | No (root depends on it) |
| `hermes-gateway` | 19 platform enum, 5 implemented adapters, session store, dedup | No |
| `hermes-cron` | Cron job scheduler, delivery, JSON job store | No |
| `hermes-batch` | JSONL batch runner, checkpointing, distributions | No |
| `hermes-compress` | Context compression and summarization | No |
| `hermes-rl` | RL environments (tool-use, web-research, math, Atropos) | No |

### Source Tree Layout

```
├── Cargo.toml              # Workspace root, shared deps, profiles
├── src/
│   ├── main.rs             # hermes binary entry point
│   ├── commands/
│   │   ├── mod.rs          # Clap CLI argument definitions (~1300 lines)
│   │   └── dispatch.rs     # Command dispatch bridge to hermes-cli handlers
│   ├── hermes_agent/
│   │   └── main.rs         # hermes-agent standalone binary
│   └── hermes_acp/
│       ├── main.rs         # hermes-acp JSON-RPC server
│       ├── protocol.rs     # ACP message types
│       ├── server.rs       # ACP method dispatch (13 methods)
│       └── session.rs      # SessionManager
├── crates/
│   └── <each crate>/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           └── <modules>.rs
├── scripts/
│   └── e2e_test.sh         # Bash E2E test suite
└── docs/
    ├── ARCHITECTURE.md     # Full 5-tier architecture diagrams
    ├── USAGE.md            # Chinese CLI usage guide
    └── E2E_TEST.md         # Chinese E2E test report
```

---

## Code Style Guidelines

### Rust Conventions Used in This Project

1. **Module visibility:** Most internal modules are `pub(crate)`. Public APIs are explicitly `pub` and re-exported in `lib.rs`.
2. **Crate-level lints:** Every `lib.rs` starts with:
   ```rust
   #![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
   ```
   Do not remove these unless you are also fixing the underlying warnings.
3. **Error handling:**
   - Use `thiserror` for structured error enums in libraries.
   - Use `anyhow` for application-level error propagation.
   - The project defines a unified `HermesError` in `hermes-core` with `ErrorCategory` for classification.
   - `Result<T>` is aliased to `std::result::Result<T, HermesError>`.
4. **Async:** All async code uses `tokio`. `async-trait` is used for trait-based async abstractions.
5. **Logging:** Use `tracing` macros (`tracing::info!`, `tracing::warn!`, `tracing::error!`). Do not use `println!` in library crates.
6. **Comments:** Write doc comments (`//!` / `///`) in English. Complex logic should have inline comments explaining *why*, not *what*.
7. **Naming:** Follow standard Rust naming (`PascalCase` for types, `snake_case` for functions/variables, `SCREAMING_SNAKE_CASE` for constants).
8. **String literals:** Prefer `format!("…")` with inline variables (Rust 1.58+ style is common, but the codebase largely uses explicit arguments).

### Things to Avoid

- Do not add speculative abstractions or unused configurability.
- Do not refactor unrelated code while fixing a bug.
- Do not delete pre-existing dead code unless explicitly asked.
- Do not change existing tool schemas or CLI argument names without checking E2E tests.

---

## Testing Instructions

### Unit / Integration Tests

Tests live in three places:
- `#[cfg(test)]` modules at the bottom of source files (most common — ~190 modules).
- Separate `tests/` directories inside crates (e.g., `crates/hermes-agent-engine/tests/`).
- Inline `#[test]` functions within source files.

Key testing patterns:
- **Mock HTTP servers:** `mockito` is used to mock LLM provider endpoints (see `agent_conversation.rs`).
- **Async tests:** Use `#[tokio::test]` for async test cases.
- **Serial tests:** Use `serial_test` when tests mutate shared state (e.g., filesystem, global registries).
- **Temp files:** Use `tempfile` crate for filesystem-related tests.

```bash
# Run tests excluding a few known flaky ones
cargo test --workspace -- --skip test_delegation_filters_blocked_toolsets \
    --skip test_build_system_prompt_basic --skip test_e2e_prompt_with_soul
```

### E2E Tests

The `scripts/e2e_test.sh` script builds the release binary and exercises ~60 CLI commands without requiring any API keys. It validates:
- Core functionality (`--version`, `--help`, `chat --help`)
- Config management (`config show`, `config check`, `setup --help`)
- Diagnostics (`doctor`, `debug`, `dump`, `logs`)
- Models & auth (`models`, `auth list`, `login --help`)
- Tools & skills (`tools list`, `skills list`)
- Sessions, backup, gateway, cron, profiles, completion

When adding a new CLI subcommand, add a corresponding `run_test` call in this script.

---

## Configuration and Runtime

### Hermes Home Directory

Default: `~/.hermes/` (override with `HERMES_HOME` env var or `--hermes-home` / `--profile` CLI flags).

```
~/.hermes/
├── config.yaml              # Main YAML config
├── .env                     # API keys (dotenv format)
├── sessions.db              # SQLite database (WAL + FTS5)
├── cron_jobs.json           # Scheduled jobs
├── webhooks.json            # Webhook subscriptions
├── .plugin_registry.json    # Plugin registry
├── skills/                  # Skill files (*.md, index.json)
├── plugins/                 # Plugin directories
└── logs/                    # Rotated log files
```

### Key Configuration Sections (`config.yaml`)

```yaml
agent:
  model: anthropic/claude-sonnet-4-20250514
  provider: anthropic
  toolsets: [filesystem, web, terminal]
compression:
  enabled: true
  target_tokens: 50
terminal:
  backend: local          # local | docker | ssh | daytona | singularity | modal
  docker_image: ubuntu:latest
```

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `HERMES_HOME` | Override data directory |
| `OPENAI_API_KEY` | OpenAI provider key |
| `ANTHROPIC_API_KEY` | Anthropic provider key |
| `OPENROUTER_API_KEY` | OpenRouter aggregator key |
| `DEEPSEEK_API_KEY` | DeepSeek key |
| `GOOGLE_API_KEY` | Google / Gemini key |

---

## Security Considerations

1. **PII Redaction:** The gateway hashes sender IDs and chat IDs before storing sessions. `hermes-core::redact` strips sensitive text from logs.
2. **Dangerous Command Approval:** Terminal and code-execution tools require user approval. A permanent allowlist is stored on disk and loaded at startup.
3. **Path Security:** File operation tools validate paths against escape attempts (symlinks, parent-directory traversal).
4. **Credential Pool:** API keys are rotated automatically on 401/402/429 errors. The pool supports multiple keys per provider.
5. **Injection Sanitization:** `hermes-prompt::injection_scan` scans context content for prompt-injection patterns before sending to the LLM.
6. **Subagent Isolation:** Subagents run with a restricted tool subset (5 blocked tools: terminal, code_exec, browser, etc.) and independent budget.

---

## Development Workflow

1. **Before coding:** Read the relevant architecture section in `docs/ARCHITECTURE.md`. It contains detailed diagrams for the agent engine, LLM provider routing, tool registry, and gateway platforms.
2. **Making changes:** Prefer minimal, surgical changes. Every changed line should trace directly to the task.
3. **After changes:** Run `cargo clippy --workspace`, `cargo test --workspace`, and `bash scripts/e2e_test.sh`.
4. **Documentation:** Update `docs/ARCHITECTURE.md` if you change tier boundaries or add new crates. Update `docs/USAGE.md` if you add user-facing CLI commands.

---

## Porting Notes (Python → Rust)

This codebase is an active port from a Python implementation. Many module doc comments reference the original Python files:
- `run_agent.py:AIAgent` → `hermes-agent-engine/src/agent.rs:AIAgent`
- `config.py:load_config` → `hermes-core/src/config.rs:HermesConfig::load`
- `toolsets.py` → `hermes-tools/src/toolsets_def.rs`
- `model_tools.py` → `hermes-tools/src/*.rs` (~60 files)
- `hermes_state.py` → `hermes-state/src/session_db.rs:SessionDB`
- `gateway/run.py` → `hermes-gateway/src/runner.rs`
- `acp_adapter/` → `src/hermes_acp/`

When in doubt about intended behavior, check the Python → Rust module mapping table in `docs/ARCHITECTURE.md` §8.

---

## Crate Feature Flags

Some crates use feature flags to gate heavy or optional dependencies:

- `hermes-tools` features: `docker`, `mcp`, `browser`, `image` (all enabled by default)
- Conditional modules use `#[cfg(feature = "...")]` (e.g., `browser`, `mcp_client`, `image_gen`)

---

## Contact / References

- Primary docs: `docs/ARCHITECTURE.md`, `docs/USAGE.md`, `docs/E2E_TEST.md`
- E2E script: `scripts/e2e_test.sh`
- Workspace root: `Cargo.toml`
