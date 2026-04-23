# hermez-rs Comprehensive Review

> Review date: 2026-04-18  
> Codebase: ~124K lines across 245 `.rs` files, 13 crates  
> Tests: ~2,039 passing, 0 failures  
> Build: All 3 binaries compile (`hermez`, `hermez-agent`, `hermez-acp`)

---

## Executive Summary

The Rust rewrite is **structurally sound** with good crate separation, comprehensive test coverage in core modules, and clean compilation. However, there are **critical async/threading bugs** that will cause deadlocks or panics in production, **security vulnerabilities** in shell execution paths, and **~12 advertised features that are non-functional stubs**. The codebase also carries **181 clippy warnings** and **~364 unwrap/expect calls** in production code, a handful of which can crash long-running services.

| Category | Severity | Count |
|----------|----------|-------|
| Async/threading bugs | 🔴 Critical | 4 |
| Security vulnerabilities | 🔴 Critical / High | 7 |
| Crash-on-error unwraps | 🔴 High | 4 |
| Non-functional stubs | 🟡 High | 12 |
| Blocking I/O in async | 🟡 Medium | 8 |
| Clippy warnings | 🟢 Low | 181 |
| Mutex poisoning exposure | 🟢 Low | ~150 |

---

## 🔴 Critical Issues (Fix Before Production)

### 1. `message_loop.rs` — Deadlock: `std::sync::Mutex` held across `.await`
**File:** `crates/hermez-agent-engine/src/message_loop.rs:63,96`

```rust
pub struct MessageLoop {
    agent: Arc<std::sync::Mutex<AIAgent>>,   // ← wrong mutex type
}

#[allow(clippy::await_holding_lock)]
pub async fn process_message(&mut self, msg: PlatformMessage) -> Result<MessageResult> {
    let mut agent = self.agent.lock().unwrap();
    agent.run_conversation(...).await   // ← lock held across entire LLM call
}
```

**Risk:** If the async executor migrates the task between threads, or if another task tries to lock the same mutex on the same thread, this **will deadlock**. The `#[allow]` acknowledges the lint but does not fix it.  
**Fix:** Replace `std::sync::Mutex` with `tokio::sync::Mutex`.

---

### 2. `docker_env.rs` — Panic on Drop in async context
**File:** `crates/hermez-tools/src/environments/docker_env.rs:99-106`

```rust
fn block_on<F, T>(f: F) -> Result<T, String> {
    let rt = Runtime::new().map_err(...)?;
    rt.block_on(f)
}
```

`Drop` calls `cleanup()` which calls `block_on()`. If dropped from within an async context (e.g., task cancellation), this **will panic** because `Runtime::new()` + `block_on()` is illegal inside an existing runtime.  
**Fix:** Make `DockerEnvironment` async-native or use `Handle::try_current()` with a fallback.

---

### 3. `skills_hub.rs` — Thread pool starvation via `block_in_place` + `block_on`
**File:** `crates/hermez-tools/src/skills_hub.rs:709`

```rust
tokio::task::block_in_place(|| {
    tokio::runtime::Handle::current().block_on(async { ... })
});
```

Can **panic or starve the thread pool** with `current_thread` runtime or when blocking threads are exhausted.  
**Fix:** Change `SkillSource` trait to `async_trait`.

---

### 4. `terminal.rs` — Async executor thread blocked for up to 600s
**File:** `crates/hermez-tools/src/terminal.rs:360-404`

Synchronous subprocess execution with `std::thread::sleep(100ms)` polling loop. Called from the async agent loop. **Blocks the executor thread** for the full command duration.  
**Fix:** Use `tokio::process::Command` or wrap in `spawn_blocking`.

---

### 5. WhatsApp bridge — Process-group kill can self-DoS
**File:** `crates/hermez-gateway/src/platforms/whatsapp.rs:376-395`

```rust
libc::kill(-(pid as i32), libc::SIGTERM);
```

If `pid` is `0`, negated value is `0`, which signals **all processes in the caller's process group**. If `pid` overflows `i32`, behavior is undefined.  
**Fix:** Validate `pid` is in `1..=i32::MAX` before negation; prefer `libc::killpg()`.

---

### 6. SSH remote execution — Command injection
**File:** `crates/hermez-tools/src/environments/ssh.rs:96-133`

```rust
format!("cd {effective_cwd} && {command}")
```

Both variables are user-controlled. Passed as single argument to `ssh`, which executes via remote shell — **shell metacharacters are interpreted remotely**.  
**Fix:** Use shell-quoting library (e.g., `shell-escape`) or pass as argument vectors.

---

### 7. Python sandbox — Trivially bypassable
**File:** `crates/hermez-tools/src/code_exec/executor.rs:32-43`

Security check is simple `code.contains(...)` for three literal strings. Easily bypassed with string concatenation, base64 decoding, etc.  
**Fix:** Use AST-based import analyzer or run in seccomp/namespace sandbox.

---

### 8. ACP server loop — Panic on serialization failure
**File:** `crates/hermez-acp/src/lib.rs:1304,1324,1333`

```rust
let bytes = serde_json::to_vec(&error_resp).unwrap();
```

Inside the main JSON-RPC message loop. Non-string JSON map keys will **panic the ACP server thread**.  
**Fix:** Replace with `match` and emit a hard-coded error frame.

---

### 9. Gateway SSE — Panic aborts HTTP stream
**File:** `crates/hermez-gateway/src/platforms/api_server.rs:1436,1981,2137,2162,2183`

```rust
fn make_event(data: impl serde::Serialize) -> Event {
    Event::default().json_data(&data).unwrap()
}
```

Panic inside `async_stream` SSE generator **aborts the HTTP response mid-flight**. 5 instances.  
**Fix:** Yield an internal-error SSE event instead of unwrap.

---

### 10. Browser tools — Panic on runtime creation failure
**File:** `crates/hermez-tools/src/browser/mod.rs` (9 instances)

```rust
tokio::runtime::Builder::new_current_thread()
    .build()
    .expect("failed to build tokio runtime")
```

Will panic in sandboxed containers, WASM, or nested runtimes.  
**Fix:** Return `HermesError` instead of expect.

---

## 🟡 High-Priority Gaps (Non-Functional Features)

| # | Feature | File | Status |
|---|---------|------|--------|
| 1 | **Modal environment** | `environments/modal.rs` | Entire skeleton — all methods return error/no-op |
| 2 | **Daytona environment** | `environments/daytona.rs` | Entire skeleton — all methods return error/no-op |
| 3 | **ACP prompt handler** | `hermez-acp/src/lib.rs:852` | Returns hardcoded placeholder; no LLM call |
| 4 | **API server SSE streaming** | `api_server.rs:1313` | Tool call events not wired through agent engine |
| 5 | **Codex streaming** | `codex.rs:1059` | Returns empty shell response |
| 6 | **Singularity `docker://` images** | `singularity.rs:411` | Not converted to SIF; most common use case broken |
| 7 | **MCP OAuth** | `mcp_oauth.rs` | Entire file is stub — zero OAuth flow implementation |
| 8 | **NeuTTS provider** | `tts.rs:213` | Permanently disabled |
| 9 | **Claw migration** | `claw_cmd.rs:260` | Only checks `~/.claude` exists; no actual migration |
| 10 | **Voice TUI recording** | `voice_tui.rs:118` | Returns empty audio bytes |
| 11 | **Cost analytics** | `insights.rs:454` | Always reports `$0.00` |
| 12 | **Gateway connected platforms** | `config.rs:275` | Parses config keys only; does not verify connectivity |

---

## 🟡 Medium-Priority Issues

### Blocking I/O in async contexts
- **`feishu.rs:565`** — `std::fs::create_dir_all` + `std::fs::write` in `async fn`
- **`discord.rs:489`** — Same pattern
- **`whatsapp.rs:267`** — `std::fs::OpenOptions` in `connect()`
- **`session.rs:396`** — `std::sync::Mutex` around SQLite DB; blocking queries in async gateway handlers
- **`runner.rs:156`** — `std::sync::Mutex` for session tracking maps; locked frequently in async handlers
- **`api_server.rs:370`** — `std::sync::Mutex` global statics accessed from async route handlers
- **`singularity.rs:25`** — `std::sync::Mutex` held during `apptainer build` (can take minutes)
- **`process_reg.rs:203`** — `std::thread::sleep` polling loop for up to 180s

**Fix:** Replace `std::fs` with `tokio::fs`; replace `std::sync::Mutex` with `tokio::sync::Mutex` or `parking_lot::Mutex`; use `spawn_blocking` for long operations.

### Large error variants (clippy)
**File:** `crates/hermez-llm/src/error_classifier.rs:63`

`ClassifiedError` contains `HashMap<String, Value>` (large). Used as `Err` variant in `Result` across ~8 high-traffic functions. Increases enum size and hurts performance.  
**Fix:** `Box<ClassifiedError>` or `Arc<ClassifiedError>`.

### Functions with too many arguments (clippy)
- `app.rs:73` — `run_chat` with **20 arguments**
- `auxiliary_client.rs:1422` — `async_call_llm` with **11 arguments**
- `api_server.rs:1418` — `build_responses_sse_stream` with **10 arguments**

**Fix:** Introduce config/params structs.

---

## 🟢 Low-Priority / Code Quality

### src/main.rs bloat
`src/main.rs` is **1,900 lines** — almost entirely clap `Subcommand` enums and dispatch logic. This should be split into subcommand modules.

### hermez-acp dual implementation
There are **two ACP implementations**:
1. `crates/hermez-acp/src/lib.rs` — crate-level skeleton (1,528 lines, placeholder `handle_prompt`)
2. `src/hermez_acp/main.rs` — binary implementation (214 lines, actual LLM wiring via `AIAgent`)

The binary is what `hermes acp run` delegates to. The crate library is misleading — it looks like the real implementation but can't process prompts.

### Dead code allowances
Many gateway platform files have `#[allow(dead_code)]` on struct fields (e.g., `dingtalk.rs:141`, `wecom.rs:87`, `weixin.rs:39`). These indicate partially implemented adapters.

### Clippy warning breakdown (181 total)
| Warning | Count |
|---------|-------|
| Deref immediately dereferenced | 19 |
| Borrowed expression implements trait | 9 |
| Large `Err` variant | 9 |
| `map_or` simplifiable | 8 |
| Too many arguments | 14 (total) |
| Lifetime elision confusion | 5 |
| Various style | ~117 |

### Test coverage distribution
| Crate | Tests | Lines | Density |
|-------|-------|-------|---------|
| `hermez-tools` | 675 | 32,529 | 1.03% |
| `hermez-llm` | 441 | 19,999 | 1.05% |
| `hermez-agent-engine` | 255 | 10,841 | 1.12% |
| `hermez-gateway` | 181 | 17,917 | 0.50% |
| `hermez-cli` | 171 | 19,676 | 0.43% |
| `hermez-acp` | 12 | 1,528 | 0.39% |
| `hermez-rl` | 22 | 3,363 | 0.32% |

`hermez-acp` and `hermez-rl` are undertested relative to their size.

---

## ✅ Positive Findings

1. **Zero production `panic!`** — All 5 `panic!` occurrences are in test blocks.
2. **Path traversal defenses** — Well-implemented in cron scheduler, credential mounts, skill directory, file ops.
3. **Device guard** — File read blocks `/dev/zero`, `/dev/urandom`, etc. to prevent infinite reads.
4. **Schema migrations** — SQLite has versioned migrations (v1→v6) in `session_db.rs`.
5. **Redaction** — `redact_sensitive_text` properly masks API keys, JWTs, connection strings.
6. **Test suite** — ~2,039 tests pass, 0 failures. Good coverage in core crates.
7. **Dependency hygiene** — No obvious unused heavy dependencies; workspace-level dependency management is clean.

---

## Recommended Priority Order

| Priority | Issue | Effort |
|----------|-------|--------|
| **P0** | Fix `message_loop.rs` deadlock (`std::sync::Mutex` → `tokio::sync::Mutex`) | Small |
| **P0** | Fix `docker_env.rs` nested runtime panic | Small |
| **P0** | Fix `skills_hub.rs` `block_in_place` + `block_on` | Medium |
| **P0** | Fix ACP server `serde_json::to_vec` unwraps | Small |
| **P0** | Fix gateway SSE `json_data` unwraps | Small |
| **P1** | Fix WhatsApp `libc::kill` process-group DoS | Small |
| **P1** | Fix SSH command injection | Medium |
| **P1** | Fix terminal tool blocking executor thread | Medium |
| **P1** | Replace `std::fs` with `tokio::fs` in gateway platforms | Small |
| **P1** | Fix browser tool runtime `expect`s | Small |
| **P2** | Box `ClassifiedError` to reduce enum size | Small |
| **P2** | Remove or hide Modal/Daytona from public registry | Small |
| **P2** | Wire ACP crate `handle_prompt` to actual LLM | Medium |
| **P3** | Strengthen Python sandbox | Medium |
| **P3** | Split `src/main.rs` into subcommand modules | Medium |
