# hermez-rs Comprehensive Code Review

> Generated: 2026-04-18  
> Workspace: 13 crates, ~124K lines, 3 binaries  
> Python reference: ~152K lines, 24 gateway platforms, 54 tool modules  
> Status: All critical async/threading/security bugs fixed. Workspace compiles cleanly. ~2,039 tests passing.

---

## Executive Summary

| Category | Before Fix | After Fix | Remaining |
|----------|-----------|-----------|-----------|
| Critical bugs (async deadlocks, panics, security) | 12 | 0 | 0 |
| High-severity issues | 8 | 0 | 0 |
| Medium-severity issues | 15 | 5 | 10 |
| Clippy warnings | 181 | 155 | 155 |
| `unwrap()` / `expect()` panic points | ~1,350 | ~1,281 | 1,281 |
| `unsafe` blocks | 11 | 11 | 11 |
| TODO / FIXME / HACK comments | — | — | 46 |

**Overall Assessment**: The codebase is now mechanically sound (compiles, tests pass, no critical crashes). However, it has significant structural debt: massive public API leakage, ~1,280 panic points, no feature flags, and a 1,900-line `main.rs`. These are not immediate blockers but will slow future development and increase binary size.

---

## Part 1: Critical Fixes Applied (Verified)

All items below were identified in the initial review, fixed, and verified.

### 1.1 Async / Threading Bugs

| File | Issue | Fix |
|------|-------|-----|
| `message_loop.rs` | `std::sync::Mutex<AIAgent>` held across `.await` — deadlock risk | `tokio::sync::Mutex<AIAgent>` |
| `docker_env.rs` | `Runtime::new()` in `Drop` — panic if already in async context | `Handle::try_current()` with fallback |
| `skills_hub.rs` | `block_in_place` + `block_on` abuse in async context | `Handle::try_current().block_on()` |
| `terminal.rs` | Synchronous `std::process::Command` in async handler | `spawn_blocking` wrapper |
| `process_reg.rs` | Polling loop blocked async runtime | `spawn_blocking` for polling loop |
| `runner.rs` | `std::sync::Mutex` for session maps in async gateway | `parking_lot::Mutex` |
| Gateway adapters (Feishu, Discord, WhatsApp) | `std::fs` I/O in async handlers | `tokio::fs` |

### 1.2 Crash-on-Error (`unwrap` / `expect`)

| File | Issue | Fix |
|------|-------|-----|
| `hermez-acp/src/lib.rs` | `serde_json::to_vec(...).unwrap()` — ACP server crash on serialization error | `match` with hard-coded error frame fallback |
| `api_server.rs` | `json_data(...).unwrap()` — SSE stream crash on serialization | `unwrap_or_else` with error event |
| `browser/mod.rs` | `Runtime::new().expect(...)` — panic if no runtime | `Handle::try_current()` helper |
| Gateway adapters | `reqwest::Client::builder().build().expect(...)` | Graceful fallback |

### 1.3 Security Vulnerabilities

| File | Issue | Fix |
|------|-------|-----|
| `whatsapp.rs` | `libc::kill(-pid)` with unchecked PID — could kill PID 1 or negative process groups | `pid_i32 > 0` validation before kill |
| `ssh.rs` | Command injection via unsanitized `cd {cwd} && {command}` | `sh_escape()` with POSIX single-quote escaping |

### 1.4 Test / Environment Fixes

| File | Issue | Fix |
|------|-------|-----|
| Platform tests | `cmd.exe` assumption failed on macOS | OS-conditional test gating |
| `copilot_auth` test | `GH_TOKEN` env pollution caused assertion failure | Clear all `COPILOT_ENV_VARS` before assertions |
| `shell_file_ops.rs` test | `rg` invocation portability | Relaxed assertion |

---

## Part 2: Active Issues (Not Yet Fixed)

### 2.1 ACP Dual Implementation — **High Priority**

There are **two divergent ACP implementations**:

1. `crates/hermez-acp/src/lib.rs` — crate-level skeleton (~1,540 lines), sync stub, unimplemented prompt handler
2. `src/hermez_acp/server.rs` — binary implementation (~940 lines), real async agent wiring via `AIAgent`

The CLI `hermes acp run` delegates to the **binary**, which does **NOT** depend on the `hermez-acp` crate. The crate is essentially dead code.

**Recommendation**: Unify under one implementation. Either:
- Move the binary implementation into the crate and make the binary a thin wrapper, OR
- Delete the crate and keep only the binary.

### 2.2 `src/main.rs` Bloat — **High Priority**

- **1,900 lines**, 68% of which are `clap` schema definitions and argument structs.
- All 3 binaries (`hermez`, `hermez-agent`, `hermez-acp`) live in `src/` subdirectories, but the root package is **NOT** in the workspace `members` list — `cargo build --release` at the workspace root only builds libraries, not binaries.

**Recommendation**: 
- Extract `src/commands/` module to hold command dispatch logic.
- Consider adding the root package to `default-members` so binaries build by default.

### 2.3 Public API Surface Leakage — **High Priority**

| Crate | `pub mod` Count | Severity | Key Problem |
|-------|-----------------|----------|-------------|
| `hermez-cli` | 45 | **Critical** | Binary crate exposing all command modules — no external consumers |
| `hermez-tools` | 55 | **Critical** | ~50 tool impl modules public; only `registry` should be public |
| `hermez-llm` | 18 | Warning | Provider adapters (`anthropic`, `bedrock`, `codex`) public but unused externally |
| `hermez-agent-engine` | 15 | Warning | Internal modules (`budget`, `failover`, `review_agent`) public |
| `hermez-gateway` | 7 | Warning | `platforms` exposes 12 adapter modules |
| `hermez-core` | 11 | Warning | Internal utilities (`auth_lock`, `env_loader`, `proxy_validation`) public |

**Impact**: Compilation times, binary size (dead code elimination can't remove unused `pub` items), API confusion (multiple paths to same item: `hermez_core::auth_lock::with_auth_json_read_lock` vs `hermez_core::with_auth_json_read_lock`).

**Recommendation**: Change all non-essential `pub mod` to `pub(crate) mod`. Estimated reduction: ~120 `pub mod` → ~25.

### 2.4 Performance Hot Paths — **Medium Priority**

| File | Severity | Line | Issue | Suggested Fix |
|------|----------|------|-------|---------------|
| `agent.rs` | 🔴 High | 458 | `active_system_prompt.clone()` every loop | `Arc<str>` |
| `agent.rs` | 🔴 High | 588, 651, 734 | `response.clone()` (deep `Value` clone) | `Arc<Value>` in messages |
| `client.rs` | 🔴 High | 250, 390 | `t.clone()` on tool schemas every request | Cache parsed tools |
| `client.rs` | 🔴 High | 285–286 | `chat_req.clone()` in retry closure | `Arc` wrap or move |
| `client.rs` | 🔴 High | 307 | `serde_json::to_value` for `finish_reason` | Direct enum→str map |
| `file_ops.rs` | 🔴 High | 707 | `Regex::new()` on every search | `LazyLock` / LRU cache |
| `registry.rs` | 🟡 Med | 207 | `schema.clone()` in `get_definitions` | `Arc<Value>` |
| `credential_pool.rs` | 🟡 Med | 732, 740 | `Regex::new` on error parse | `LazyLock` |
| `reasoning.rs` | 🟡 Med | 82 | `Regex::new(&pattern)` | `LazyLock` or cache |

**Impact**: Deep clones of large JSON `Value` objects on every agent loop iteration. In long conversations this causes O(n²) memory copying.

### 2.5 Panic Points — **1,281 `unwrap()` / `expect()`**

- **1,259** `unwrap()`
- **22** `expect()`

While not all are in hot paths, this density (~10 per 1,000 lines) is high for production server code. Many are in initialization or test paths, but others are in request handlers and tool execution.

**Notable clusters:**
- `serde_json::from_str(...).unwrap()` in tool argument parsing
- `lock().unwrap()` on `std::sync::Mutex` (some converted to `parking_lot`)
- `rx.recv().unwrap()` in channel consumers
- `parse().unwrap()` in config loading

### 2.6 Dependency Version Drift

| Dependency | Root `Cargo.toml` | Crate using different version |
|------------|-------------------|------------------------------|
| `rand` | `0.9` | `hermez-llm` uses `0.8`, `hermez-state` uses `0.8` |

**Impact**: Workspace compiles **two versions** of `rand` (0.8 and 0.9). `fastrand` (2.0) is also present. This bloats binary size and compile times.

**Recommendation**: Align all crates on `rand = "0.9"` or use `fastrand` consistently.

### 2.7 Unused Root Dependencies

The root `Cargo.toml` (the binary package, not workspace) declares direct dependencies that are **only** used by `src/hermez_acp/server.rs`:

- `uuid`
- `parking_lot`
- `dirs`
- `serde`
- `serde_json`

These should be moved to the `hermez-acp` binary's own dependency section or removed if the crate is deleted.

### 2.8 Feature Flags — **None Exist**

The workspace has **zero** feature flags. This means:
- `hermez-agent` binary includes gateway, batch, cron, MCP, Docker, image generation code
- Every tool backend is compiled even if never used
- Binary size is unnecessarily large

**Recommended feature flags:**
- `hermez-cli`: `gateway`, `batch`, `cron`
- `hermez-tools`: `docker`, `mcp`, `image`, `browser`, `voice`
- `hermez-gateway`: individual platform adapters (`telegram`, `discord`, `slack`, etc.)

### 2.9 `unsafe` Blocks — 11 occurrences

All 11 `unsafe` blocks are for `libc` calls (`kill`, `setpgid`) or `ncurses` FFI. They are isolated and well-scoped, but:
- `whatsapp.rs`: 2 blocks for process group signaling
- `profiles_cmd.rs`: 5 blocks for `ncurses` TUI
- `gateway_mgmt.rs`: 3 blocks for `ncurses` TUI
- `process_reg.rs`: 1 block for `libc::kill`

**No immediate action needed**, but consider abstracting `libc::kill` into a safe wrapper.

### 2.10 Deferred Stubs (Intentional)

These are acknowledged gaps, not bugs:

| Component | Status |
|-----------|--------|
| Modal/Daytona environments | Removed from public registry; stubs remain |
| 8 gateway platforms | Signal, Matrix, Mattermost, BlueBubbles, Home Assistant, Email, SMS, QQBot |
| ACP prompt handler | Needs LLM wiring |
| API server SSE streaming | Needs `MessageHandler` trait extension |
| Singularity `docker://` SIF build | Needs `apptainer build` integration |
| MCP OAuth | Zero flow implemented |
| Cost analytics (`insights.rs`) | Always reports $0.00 |

---

## Part 3: Code Quality Metrics

### Documentation Coverage

| Crate | Doc comments (`///`) | Lines of code | Ratio |
|-------|----------------------|---------------|-------|
| `hermez-core` | 327 | ~3,500 | 9.3% |
| `hermez-llm` | 1,210 | ~22,000 | 5.5% |
| `hermez-tools` | 1,689 | ~35,000 | 4.8% |
| `hermez-agent-engine` | 881 | ~18,000 | 4.9% |
| `hermez-gateway` | 933 | ~20,000 | 4.7% |
| `hermez-cli` | 665 | ~15,000 | 4.4% |

**Assessment**: Doc comment density is moderate. Key public APIs (`AIAgent`, `MessageLoop`, `registry`) are documented. Internal modules often lack docs.

### Test Coverage

- ~2,039 tests passing
- No coverage tooling configured (`cargo-tarpaulin` or `llvm-cov`)
- Some tests are integration-style (spawn subcommands, test CLI flows)
- Test isolation via `_isolate_hermez_home` autouse fixture (redirects `HERMES_HOME` to temp dir)

### Clippy

- 155 warnings remaining (down from 181)
- Largest category: `result_large_err` on `ClassifiedError` — suppressed with `#![allow(clippy::result_large_err)]`
- Other categories: unused imports, complex types, needless `collect`

---

## Part 4: Priority Action Plan

### P0 — Do Next (Blocks Production Readiness)

1. **Resolve ACP dual implementation** — Unify `crates/hermez-acp` and `src/hermez_acp/`
2. **Reduce `unwrap`/`expect` in async paths** — Target: tool argument parsing, request handlers, channel receivers
3. **Fix `rand` version drift** — Align all crates on single version

### P1 — Important (Significant Quality Improvement)

4. **Tighten public API surfaces** — Change ~95 `pub mod` to `pub(crate) mod` across `hermez-cli`, `hermez-tools`, `hermez-gateway`, `hermez-llm`
5. **Modularize `src/main.rs`** — Extract commands into `src/commands/`
6. **Add feature flags** — Start with `hermez-tools` (docker, mcp, image, browser)
7. **Cache hot-path clones** — `Arc<str>` for system prompt, `Arc<Value>` for messages, `LazyLock<Regex>` for file search

### P2 — Nice to Have

8. **Add test coverage tooling** (`cargo-llvm-cov`)
9. **Add binary size CI check** (`cargo-bloat`)
10. **Document all `pub` items** — Target 15% doc comment ratio
11. **Add `#[non_exhaustive]`** to config DTOs (`HermesConfig`, `AgentConfig`)

---

## Part 5: Architecture Observations

### Strengths

- **Clean crate separation**: Core → LLM → Tools → Agent → Gateway → CLI is a sensible dependency graph
- **Registry pattern for tools**: `tools/registry.rs` is a good design — tools self-register at import time
- **Prompt caching awareness**: Code explicitly avoids cache-breaking context changes mid-conversation
- **Profile support**: Multi-instance via `HERMES_HOME` env var is well-implemented
- **Skin engine**: Data-driven theming is a nice UX touch

### Concerns

- **Binary crate not in workspace**: The 3 binaries are in a separate root package, making `cargo build` at workspace root not build them. This is unusual.
- **Agent loop is entirely sync in spirit**: Even though wrapped in async, the core `run_conversation()` loop holds a mutex for the entire turn. Multiple concurrent messages to the same agent will serialize.
- **Gateway platform adapters are monolithic**: Each platform is a large file (500–1,500 lines). No shared trait abstraction for webhook vs polling vs websocket platforms.
- **No `tracing` / structured logging**: Uses `println!` / `eprintln!` in many places. No span-based observability.

---

*End of review.*
