# Upstream Alignment Report — hermez-ai vs hermes-agent

> **Review date**: 2026-04-25
> **Baseline**: hermes-agent `3e652f75` (2026-04-21)
> **Upstream delta**: 544 substantive commits (669 total, less chore/docs/merge)
> **hermez-ai delta since baseline**: 10 commits (renames, refactoring, test additions)
> **hermez-ai current state**: 12 crates, ~163K lines, 25 gateway platform files

---

## Executive Summary

The Python upstream has received **544 substantive commits** since the last alignment baseline (2026-04-21). These span 19 functional modules, with the heaviest activity in LLM providers (~75 commits), agent engine (~55), tools/MCP (~50), gateway platforms (~45), and security (~25).

**hermez-ai has not tracked these changes.** Its 10 commits since baseline are all internal cleanups (hermes→hermez rename, test additions, utility deduplication). No upstream feature alignment work has occurred.

The upstream changes fall into three tiers:

| Tier | Count | Description |
|------|-------|-------------|
| **P0 — Bug fixes the Rust port likely shares** | ~120 | Provider quirks, MCP robustness, session correctness, security hardening |
| **P1 — New features worth porting** | ~25 | Cron context_from, Discord tool split, SessionSource expansion, per-job config |
| **P2 — Python-specific or TUI/web only** | ~30 | Voice/TUI, dashboard themes, Docker deployment, FHS installer |

### Overall Alignment Estimate

| Dimension | Before (2026-04-21) | After This Report | Target |
|-----------|---------------------|-------------------|--------|
| Functional parity | ~75% | ~75% (unchanged) | 85%+ |
| Bug fix parity | ~60% | ~60% (gaps identified) | 90%+ |
| Provider robustness | ~70% | ~70% (40+ fixes missing) | 90%+ |
| Security hardening | ~75% | ~75% (8 fixes missing) | 95%+ |

---

## Part 1: Critical Alignment Items (P0 — Must Port)

These are upstream bug fixes for failure modes that exist in the Rust port's architecture. They are not Python-specific — they will manifest in Rust too.

### 1.1 LLM Provider Robustness (~40 fixes)

**DeepSeek reasoning echo** (`93a2d6b3`)
- DeepSeek requires `reasoning_content` echoed back on every tool-call message in the conversation history. Without it, the API returns errors.
- **Rust status**: Unknown — check `crates/hermez-llm/src/anthropic.rs` and any DeepSeek-specific handling.
- **Effort**: Medium (requires understanding DeepSeek API contract)

**Kimi /coding thinking block preservation** (`04e039f6`, `2efb0eea`)
- Kimi's reasoning blocks have specific ordering requirements. `reasoning_content` must be preserved on assistant tool-call messages.
- **Rust status**: Likely missing. No Kimi-specific code found.
- **Effort**: Medium

**Temperature stripping on retry** (`facea845`, `f67a61dc`)
- Some providers reject the `temperature` parameter. The retry/fallback path must strip it.
- **Rust status**: Check `crates/hermez-llm/src/retry.rs`.
- **Effort**: Small

**Codex OAuth token lifecycle** (`346601ca`, `51f4c982`, `813dbd9b`, `1f9c3686`)
- Stale OAuth cache entries must be invalidated (>=400KB threshold)
- Real context window is 272K, not 1M (4x overestimate)
- Auth failures must route to fallback provider chain
- OpenAPI nested error shapes must be parsed in token refresh
- Codex setup must reuse or reauthenticate
- **Rust status**: Check `crates/hermez-llm/src/codex.rs`.
- **Effort**: Medium-Large

**Bedrock robustness** (`b290297d`, `a9ccb03c`, `f2fba4f9`, `7c3e5706`, `7dc6eb9f`)
- Cached boto3 client must be evicted on stale-connection errors
- Context length resolved via static table before custom-endpoint probe
- Auto-detect Bedrock model IDs in normalize_model_name
- Bedrock-aware client rebuild on interrupt
- Handle aws_sdk auth type
- **Rust status**: Check `crates/hermez-llm/src/bedrock.rs` (1,798 lines).
- **Effort**: Medium

**Copilot auth recovery** (`2cab8129`, `d7ad07d6`, `76329196`)
- 401 auth recovery with automatic token refresh and client rebuild
- Exchange raw GitHub token for Copilot API JWT
- Wire live /models max_prompt_tokens into context-window resolver
- **Rust status**: Check `crates/hermez-llm/src/copilot_acp_client.rs`.
- **Effort**: Medium

**Gemini schema sanitization** (`1f9c3686`, `04e039f6`)
- Gemini rejects integer/number/boolean enums in tool schemas — must drop them
- Block free-tier keys at setup with guidance on 429 errors
- Fail fast on missing API key
- **Rust status**: No Gemini adapter exists in hermez-ai at all. **This is a complete gap.**
- **Effort**: Large (new provider adapter needed)

**Other provider fixes:**
- `4ac731c8` — Pass DeepSeek V-series IDs through instead of folding to deepseek-chat
- `5383615d` — Recognize Claude Code OAuth tokens (cc- prefix)
- `be6b8356` — Force Anthropic OAuth refresh after 401
- `e1106772` — Re-auth on stale OAuth token; read from macOS Keychain
- `56086e3f` — Write Anthropic OAuth token files atomically
- `ba44a3d2` — Gemini: fail fast on missing API key
- `f5af6520` — Add extra_content property to ToolCall for Gemini thought_signature
- `d74eaef5` — Retry mid-stream SSL/TLS alert errors as transport
- `2acc8783` — Classify OpenRouter privacy-guardrail 404s distinctly
- `77e04a29` — Don't classify generic 404 as model_not_found
- `3d90292e` — Normalize provider in list_provider_models for aliases
- `8f5fee3e` — Add gpt-5.5 and wire live model discovery
- `9fde22d2` — Fix /model command resetting model change
- `05d8f110` — Show provider-enforced context length, not raw models.dev
- `a9ccb03c` — Evict cached Bedrock client on stale connections
- `4e27e498` — Exclude ssl.SSLError from is_local_validation_error
- `f2fba4f9` — Auto-detect Bedrock model IDs

### 1.2 Agent Engine Robustness (~15 fixes)

**Fallback chain edge cases:**
- `1fc77f99` — Fall back on rate limit when credential pool has no rotation room
- `46451528` — Pass config_context_length in fallback activation path
- `e020f46b` — Preserve MiniMax context length on delta-only overflow
- `a9fd8d7c` — Default missing fallback chain on switch
- **Rust status**: Check `crates/hermez-agent-engine/src/failover.rs` (474 lines).
- **Effort**: Small-Medium

**Context compression on model switch** (`5401a008`)
- Switching models via /model doesn't recalculate compressed context budgets. The old budget persists, causing overflow or underflow.
- **Rust status**: Check `crates/hermez-prompt/src/context_compressor.rs`.
- **Effort**: Small

**JSON decode error classification** (`c2b3db48`)
- `json.JSONDecodeError` in agent responses must be treated as retryable (not local validation error). Currently causes non-retryable abort.
- **Rust status**: Check `crates/hermez-llm/src/error_classifier.rs`.
- **Effort**: Small

**Malformed tool call repair** (`7a192b12`, `2d444fc8`, `17fc84c2`)
- Streaming assembly can produce corrupted JSON arguments with unescaped control chars
- Must repair before sending to provider, not just flag as truncated
- CamelCase + `_tool` suffix tool-call format from some models must be normalized
- **Rust status**: Check `crates/hermez-llm/src/tool_call/` parsers.
- **Effort**: Medium

**Heartbeat/tool-activity awareness** (`fcc05284`)
- Subagent heartbeat only counted API calls, not tool execution time. Subagents silently timeout during long tool runs.
- **Rust status**: Check `crates/hermez-agent-engine/src/subagent.rs` (609 lines).
- **Effort**: Small

**Empty content acceptance** (`b49a1b71`)
- Some providers return empty content blocks with `stop_reason=end_turn`. These must be accepted as valid.
- **Rust status**: Check Anthropic response parsing.
- **Effort**: Small

**Memory skip on interrupt** (`00c3d848`)
- Memory sync on Ctrl+C can corrupt state. External provider sync must be skipped on interrupted turns.
- **Rust status**: Check `crates/hermez-agent-engine/src/memory_manager.rs`.
- **Effort**: Small

**Additional agent engine items:**
- `7634c138` — Diagnostic dump when subagent times out with 0 API calls
- `165b2e48` — Make API retry count configurable via config
- `d1ce3586` — Add PLATFORM_HINTS for matrix, mattermost, feishu
- `4ac1c959` — Resolve fallback provider key_env secrets

### 1.3 MCP Protocol Robustness (~10 fixes)

**Auto-reconnect + retry** (`e87a2100`)
- MCP transport sessions expire. Must auto-reconnect and retry once on session expiry.
- **Rust status**: Check `crates/hermez-tools/src/mcp_client/`.
- **Effort**: Medium

**Stderr isolation** (`379b2273`)
- MCP stdio subprocess stderr must route to log file, not user TTY. Otherwise MCP server stderr spam floods the user terminal.
- **Rust status**: Check MCP client stdio handling.
- **Effort**: Small

**Cross-origin auth stripping** (`8c2732a9`)
- MCP auth headers must be stripped on cross-origin redirect to prevent credential leaks.
- **Rust status**: Check MCP HTTP transport.
- **Effort**: Small

**Schema sanitization** (`24f139e1`, `9ff21437`)
- MCP input schemas may contain `$defs` refs that must be rewritten to `$defs`
- Stringified arrays/objects in tool args must be coerced
- **Rust status**: Check MCP schema handling.
- **Effort**: Small

**Additional MCP items:**
- `3ccda2aa` — Seed protocol header before HTTP initialize
- `67c8f837` — Per-process PID isolation prevents cross-session crash on restart
- `b80b400b` — Respect ssl_verify config for StreamableHTTP servers
- `5fa2f425` — Serialize Pydantic AnyUrl fields when persisting OAuth state
- `34c3e671` — Sanitize tool schemas for llama.cpp backends

### 1.4 Security Hardening (~8 fixes)

All of these must be verified in the Rust port:

- `8c2732a9` — Strip MCP auth on cross-origin redirect (credential leak)
- `0e235947` — Honor security.redact_secrets from config.yaml
- `1dfcda4e` — Guard env and config overwrites in approval
- `56086e3f` — Write OAuth token files atomically to prevent corruption
- `c599a41b` — Preserve corrupt auth.json and warn instead of silently resetting
- `5eefdd9c` — Skip non-API-key auth providers in env-var credential detection
- `785d168d` — Cross-process auth-store sync for multi-gateway deployments
- `cd221080` — Validate auth status against runtime credentials

### 1.5 Gateway Session Correctness (~10 fixes)

- `d72985b7` — Serialize reset command handoff and heal stale session locks (split-brain)
- `b7bdf32d` — Guard session slot ownership after stop/reset
- `d0821b05` — Only clear locks belonging to the replaced process
- `97b9b3d6` — Drain-aware update + faster still-working pings
- `3e6c1085` — Honor queue mode in runner PRIORITY interrupt path
- `36730b90` / `050aabe2` — Clear session-scoped approval state on /new and session boundary
- `5651a733` — Guard-match the finally-block _active_sessions delete
- `260ae621` — Invoke session finalize hooks on expiry flush
- `10deb1b8` — Canonicalize WhatsApp identity in session keys

### 1.6 Tool Execution Correctness (~8 fixes)

- `5a26938a` — Auto-source ~/.profile and ~/.bash_profile so PATH survives
- `435d86ce` — Use builtin `cd` in command wrapper to bypass shell aliases
- `c47d4eda` — Restrict RPC socket permissions to owner-only
- `ea67e495` — Silent retry when stream dies mid tool-call
- `c345ec9a` — Strip standalone tool-call XML tags from visible text
- `284e084b` — Browser: upgrade agent-browser 0.13 → 0.26, wire daemon idle timeout
- `be99feff` — Image-gen: force-refresh plugin providers in long-lived sessions
- `bace220d` — Image-gen: persist plugin provider on reconfigure

---

## Part 2: High Priority Feature Gaps (P1 — Should Port)

### 2.1 Cron: Context Chaining + Per-Job Config

- `5ac53659` — **context_from**: Jobs can chain outputs from other jobs. Adds a `context_from` field to job config and wires it through the update action.
- `852c7f3b` — **Per-job workdir**: Project-aware cron runs with custom working directory
- `0086fd89` / `8b79acb8` / `ef5eaf8d` — **Per-job enabled_toolsets**: Reduce token overhead by selecting tools per job

**Rust status**: None of these exist in `crates/hermez-cron/` (2,408 lines). Confirmed no `context_from`, `workdir`, or per-job toolset filtering.

**Effort**: Medium (context_from: ~200 lines; workdir: ~50 lines; toolsets: ~100 lines)

### 2.2 Discord: Tool Split + SessionSource Expansion

- `81987f03` — Split `discord_server` into `discord` + `discord_admin` tools
- `6ed37e0f` — Make discord/discord_admin opt-in, Discord-only
- `0702231d` / `47b02e96` — Add `guild_id`, `parent_chat_id`, `message_id` to SessionSource
- `591deeb9` — Inject Discord IDs block when discord tool is loaded
- `b61ac896` — Read permission attrs from AppCommand, canonicalize contexts
- `a1ff6b45` — Safe startup slash sync policy
- `8a1e247c` / `8598746e` — Honor wildcard '*' in channel configs

**Rust status**: Check `crates/hermez-gateway/src/platforms/discord.rs` (1,754 lines). SessionSource likely needs struct expansion.

**Effort**: Medium (tool split: ~300 lines; SessionSource: ~50 lines; wildcard: ~20 lines)

### 2.3 Feishu: Doc/Drive Tools Composite

- `db09477b` — Wire feishu doc/drive tools into hermes-feishu composite tool

**Rust status**: Check `crates/hermez-gateway/src/platforms/feishu.rs` (3,339 lines).

**Effort**: Medium

### 2.4 Config Parsing Robustness

- `8ea389a7` — Coerce quoted boolean values in config parsing
- `fd9b692d` / `bfa60234` — Tolerate null top-level sections in config.yaml
- `a5b0c7e2` — Preserve list-format models in custom_providers normalize
- `7897f65a` — Lowercase Xiaomi model IDs for case-insensitive config

**Rust status**: Check `crates/hermez-core/src/config.rs`. The Rust config parser (serde_yaml) handles some of these differently than Python's pyyaml.

**Effort**: Small

### 2.5 Model Additions

Add these models to the Rust model registry:
- `openai/gpt-5.5` and `gpt-5.5-pro` (`db9d6375`)
- `deepseek-v4-pro` and `deepseek-v4-flash` (`2e78a2b6`)
- `Xiaomi MiMo v2.5-pro` and `v2.5` (`82a0ed1a`)
- `minimax/minimax-m2.5:free` (already in baseline)

**Rust status**: Check `crates/hermez-llm/src/models_dev.rs` and `crates/hermez-llm/src/model_metadata.rs`.

**Effort**: Small

### 2.6 Gateway Platform Fixes

- `3cf13747` — Matrix: bind PgCryptoStore device_id for fresh E2EE installs
- `e7590f92` — Telegram: honor no_proxy for explicit proxy setup
- `19a3e2ce` — Follow compression continuations during /resume
- `f24956ba` — Redirect --resume to the descendant that holds the messages
- `327b57da` — Kill tool subprocesses before adapter disconnect on drain timeout
- `f731c2c2` — BlueBubbles: align iMessage delivery with non-editable UX

**Effort**: Small per fix

---

## Part 3: Medium Priority Items (P2 — Nice to Port)

### 3.1 CLI UX Improvements

- `fd3864d8` / `1dcf79a8` — Busy input mode (block input during /compress)
- `08d5c9c5` — Ctrl+D behavior matches bash/zsh (delete char, exit on empty)
- `ed91b79b` — Keep Ctrl+D no-op when only attachments pending
- `a2a8092e` — Add --ignore-user-config and --ignore-rules flags

### 3.2 New Features

- Spotify tools with PKCE auth (`7e9dd9ca`, `e5d41f05`, `8d12fb1e`, `1840c6a5`, `05394f2f`)
- Telegram chat allowlists for groups/forums (`591aa159`)
- Plugin pre_gateway_dispatch hook (`1ef1e4c6`)
- Expose plugin slash commands on all platforms (`51ca5759`)
- Gemini free-tier key blocking (`3aa1a41e`)
- Browser CDP supervisor — dialog detection + cross-origin iframe eval (`5a1c5994`)
- xAI Grok STT provider (`a6ffa994`)
- xAI image generation provider (`a5e4a86e`)

### 3.3 Prompt/Preservation

- Reasoning content preservation for Kimi/DeepSeek in tool-call messages
- Stream thinking + tools expanded by default in TUI (`67bfd4b8`)
- Guard context compressor against structured message content (`1e8254e5`)

---

## Part 4: Low Priority / Deferred (P3)

### Python-Specific (No Port Needed)

- `023b1bff` — Delegate subagent approval deadlock: Python ThreadPoolExecutor + threading.local() issue. **N/A for Rust** (tokio tasks carry context correctly).
- FHS installer layout for Linux (`f433197f`) — deployment concern, not logic
- Docker deployment fixes (`acd78a45`, `14c9f727`, etc.) — deployment concern

### TUI/Voice Only (Rust TUI is different)

- Voice TTS-STT feedback loop fixes
- TUI mouse disable on ConPTY
- FloatingOverlay visibility fixes
- Xterm.js resize settle dance
- VAD-continuous recording model

### Web Dashboard Only

- PTY WebSocket bridge (`f49afd31`)
- Theme/plugin extension points (`f593c367`)
- Font/layout/density themes (`255ba5bf`)
- Mobile chat layout (`63975aa7`)

---

## Part 5: Pre-Existing hermez-ai Gaps (Not Upstream-Related)

These are gaps in the Rust port that exist regardless of upstream changes:

### 5.1 Critical (already in codebase, just not wired)

| Gap | Location | Status |
|-----|----------|--------|
| API Server SSE streaming not wired | `api_server.rs:1315` | `StreamCallback` wired but `stream_delta_callback` TODO |
| Approval blocking not connected | `feishu.rs:1860`, `slack.rs:914` | `ApprovalRegistry` exists, platform hooks not wired |
| QQBot voice/STT stub | `qqbot.rs:676` | TODO stub |

### 5.2 Complete Feature Gaps

| Gap | Description |
|-----|-------------|
| **Gemini / Google Code Assist adapter** | No Rust implementation exists. Upstream has `agent/gemini_cloudcode_adapter.py` (845 lines), `agent/google_code_assist.py` (220 lines), `agent/gemini_schema.py` (93 lines) |
| **xAI Grok provider** | STT + image generation providers not in Rust |
| **Spotify tools** | New upstream feature (tools + skill + setup wizard, ~5 commits) |
| **Daytona environment** | 348-line stub in `environments/daytona.rs` (unreachable from registry) |
| **Modal environment** | 360-line stub in `environments/modal.rs` (unreachable from registry) |

### 5.3 Code Quality Items (from COMPREHENSIVE_REVIEW.md, still relevant)

- ~1,281 `unwrap()`/`expect()` calls in production code
- 155 clippy warnings
- No feature flags for optional backends
- No auto-prune/VACUUM in state crate
- `#[allow(dead_code)]` on many gateway platform files (weixin: 12, wecom: 11, dingtalk: 2, etc.)

---

## Part 6: Effort Summary

### P0 — Critical Bug Fixes (est. 8-12 days)

| Group | Fixes | Est. Effort |
|-------|-------|-------------|
| LLM Provider bugs | ~25 fixes across 7 providers | 4-5 days |
| Agent engine fallback/retry | ~8 fixes | 1-2 days |
| MCP protocol robustness | ~10 fixes | 2 days |
| Security hardening | ~8 fixes | 1 day |
| Gateway session correctness | ~10 fixes | 1-2 days |
| Tool execution correctness | ~8 fixes | 1 day |

### P1 — Feature Gaps (est. 5-8 days)

| Group | Features | Est. Effort |
|-------|----------|-------------|
| Cron context_from + per-job config | 3 features | 1-2 days |
| Discord tool split + SessionSource | 7 items | 1-2 days |
| Feishu doc/drive composite | 1 item | 0.5 day |
| Config parsing robustness | 4 items | 0.5 day |
| Model additions | ~5 models | 0.5 day |
| Gateway platform fixes | ~6 fixes | 1 day |
| Google/Gemini adapter | New provider | 2-3 days |

### P2 — Nice to Have (est. 3-5 days)

| Group | Items | Est. Effort |
|-------|-------|-------------|
| CLI UX improvements | 4 items | 1 day |
| Reasoning preservation | 3 items | 1 day |
| New features (Spotify, Telegram allowlists, etc.) | ~6 items | 2-3 days |

---

## Part 7: Recommended Porting Sequence

### Week 1: Core Stability

1. **LLM Provider bug fixes** — Start with the most impactful: temperature stripping, DeepSeek reasoning echo, Kimi block ordering, JSON decode error classification, empty content acceptance
2. **Agent engine fallback fixes** — Fallback chain defaults, config_context_length passthrough, rate-limit fallback
3. **Context compression on model switch** — Recalculate budgets when model changes

### Week 2: Protocol & Security

4. **MCP robustness** — Auto-reconnect, stderr isolation, schema sanitization, auth stripping, PID isolation
5. **Security hardening** — Atomic OAuth writes, redact_secrets config, credential detection, cross-process sync
6. **Tool execution fixes** — PATH sourcing, builtin cd, RPC socket permissions, streaming retry

### Week 3: Gateway & Features

7. **Gateway session correctness** — Split-brain protection, lock ownership, drain-aware updates, session finalize hooks
8. **Cron context_from + per-job config**
9. **Discord tool split + SessionSource expansion**
10. **Feishu doc/drive composite**

### Week 4: Completeness

11. **Google/Gemini adapter** (new provider)
12. **Config parsing robustness**
13. **Model additions**
14. **Remaining gateway platform fixes**

---

## Part 8: Verification Checklist

For each item ported, verify:

- [ ] Does the fix apply to Rust's architecture? (Not Python-specific)
- [ ] Is there a test that would catch regression?
- [ ] Does the Rust port already handle this? (Check before implementing)
- [ ] Are there knock-on effects in other crates?

### Quick Wins (Small Effort, High Impact)

These can be done immediately:

1. Add `context_from` field to cron job struct — feature gap, ~50 lines
2. Coerce quoted booleans in config parsing — robustness, ~10 lines  
3. Add GPT-5.5, DeepSeek V4, MiMo model entries — model registry, ~20 lines
4. Strip temperature on retry when provider rejects it — retry.rs, ~15 lines
5. Accept empty content with stop_reason=end_turn — anthropic.rs, ~5 lines
6. Classify JSON decode errors as retryable — error_classifier.rs, ~5 lines
7. Strip standalone tool-call XML tags from visible text — prompt processing, ~10 lines

---

*End of report.*
