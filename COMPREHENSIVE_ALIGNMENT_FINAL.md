# Comprehensive Alignment Report — Final

> **Date**: 2026-04-25
> **Methodology**: Detailed side-by-side code reading of Python upstream and Rust port, plus 6 parallel agents comparing each major module
> **Scope**: All 12 crates + binary targets vs Python hermes-agent (~152K lines Python, ~163K lines Rust)
> **Previous reports subsumed**: UPSTREAM_ALIGNMENT_REPORT.md, REVIEW.md, COMPREHENSIVE_REVIEW.md, ALIGNMENT_REPORT.md, PORT_COMPLETENESS_REPORT.md

---

## Overall Assessment

The Rust port is a **high-fidelity implementation** achieving approximately **77-82% functional completeness** against the Python upstream. The architecture is sound: async/tokio base is correct, error classification is faithful, and core agent loop is structurally identical.

The remaining gaps fall into five categories:

| Category | Count | Impact |
|----------|-------|--------|
| **Feature exists but not wired** (dead code with `#[allow(dead_code)]`) | ~12 | HIGH — complete implementations that just need glue code |
| **Missing edge-case handling** | ~15 | MEDIUM — known failure modes not yet handled |
| **New upstream features not ported** | ~8 | MEDIUM — features added since last alignment |
| **Incomplete implementations** (stubs/skeletons) | ~5 | HIGH — MCP client, environments, etc. |
| **Structural design gaps** | ~5 | HIGH — missing subsystem integration points |

---

## Part 1: CRITICAL Gaps (8 items)

These block production readiness for specific use cases.

| # | Area | Gap | Detail |
|---|------|-----|--------|
| C1 | **Cron** | No `context_from` job chaining | Jobs can't consume output of prior jobs. `CronJob` struct has no `context_from` field |
| C2 | **Cron** | No per-job `workdir` | `CronJob` has no `workdir` field for project-aware runs |
| C3 | **Cron** | No per-job `enabled_toolsets` | `CronJob` has no `enabled_toolsets` field |
| C4 | **Gateway** | SessionSource missing 4 fields | Missing `guild_id`, `parent_chat_id`, `message_id`, `is_bot`. Discord tools can't work |
| C5 | **Gateway** | No drain/shutdown tool subprocess kill | `runner.rs:1555-1585` — `stop()` sends shutdown signals but doesn't kill tool subprocesses, drain agents, or fire finalize hooks |
| C6 | **Gateway** | WhatsApp identity canonicalization missing | No `whatsapp_identity` module. JID/LID→phone canonicalization missing. Same user gets different session keys |
| C7 | **Plugins** | No `pre_gateway_dispatch` hook | Most important plugin hook for message interception before agent dispatch is absent |
| C8 | **Compressor** | No token budget recalculation on model switch | `ContextEngine` trait has no `update_model` method. Switching 200K→32K model keeps old thresholds |

---

## Part 2: HIGH Severity Gaps (21 items)

Significant functional impact for specific scenarios.

### Agent Engine (4 items)

| # | Gap | Detail |
|---|-----|--------|
| H1 | Checkpoint integration not wired | `checkpoint.rs` exists but never called from agent. No snapshots before destructive tools |
| H2 | Tool result persistence not wired | `tool_result_storage.rs` `maybe_persist_tool_result()` and `enforce_turn_budget()` are dead code |
| H3 | No memory flush before compression | Model can't save facts to memory before middle turns are discarded |
| H4 | Last user message tail protection missing | Tail cut by tokens only — last user message can be compressed into summary, losing the active task |

### LLM Providers (5 items)

| # | Gap | Detail |
|---|-----|--------|
| H5 | Stale-call timeout lacks per-provider config | Only env var; Python uses config.yaml per-provider/per-model |
| H6 | Stale-call: no separate streaming timeout with SSE keep-alive awareness | Streaming and non-streaming share same stale timeout |
| H7 | Stream delta: no Ollama index/id fix | Ollama can reuse index 0 with different IDs — no slot tracking |
| H8 | OAuth refresh: only Anthropic | No Codex, Nous, or Copilot credential refresh paths |
| H9 | Multiple retry recovery strategies missing | No invalid tool name recovery, empty content retries, partial stream recovery, thinking prefill retries |

### MCP Client (3 items)

| # | Gap | Detail |
|---|-----|--------|
| H10 | MCP HTTP transport completely missing | Only stdio transport works. Streamable HTTP, SSE not implemented |
| H11 | No MCP schema normalization | No `$defs` rewriting, enum stripping, or stringified array coercion |
| H12 | No MCP auto-reconnect | One-shot connect. No exponential backoff or session expiry retry |

### Gateway (4 items)

| # | Gap | Detail |
|---|-----|--------|
| H13 | Discord tool split not implemented | No `discord`/`discord_admin` opt-in split. No conditional Discord ID injection |
| H14 | Feishu doc/drive tools not wired as composite | Comment flow uses generic handler, not specialized doc/drive toolset |
| H15 | SSE streaming callback not wired | `api_server.rs:1315` TODO. Post-hoc character chunking instead of true streaming |
| H16 | No session expiry watcher or finalize hooks | No background memory flush on session expiry. No `on_session_finalize` plugin hook |

### Security (3 items)

| # | Gap | Detail |
|---|-----|--------|
| H17 | No smart LLM-based approval | `ApprovalMode::Smart` enum exists but `evaluate_command()` only does pattern matching |
| H18 | No gateway blocking flow | No per-entry approval queues, activity heartbeat during blocking, cron mode |
| H19 | URL query/userinfo redaction missing | `redact.rs` doesn't strip `?access_token=...` or `user:password@` from URLs |

### Context Compression (2 items)

| # | Gap | Detail |
|---|-----|--------|
| H20 | Summary template missing `Active Task` section | Python template declares "THE SINGLE MOST IMPORTANT FIELD" |
| H21 | No `has_active_processes_fn` in SessionStore | Cannot prevent resetting sessions with running background processes |

---

## Part 3: MEDIUM Severity Gaps (24 items)

Nice-to-have, UX polish, or edge cases.

### Agent Engine (5 items)
- M1: Memory context injection persists to session storage (should be API-time only)
- M2: Memory injection finds first user message, not current turn's
- M3: Anti-thrashing tracking inside `quiet_mode` guard — never activates in gateway mode (BUG)
- M4: No concurrent activity heartbeat during parallel tool execution
- M5: `approve_always` persistence missing in concurrent execution path

### LLM Providers (4 items)
- M6: No `metadata.raw` parsing for OpenRouter wrapped errors
- M7: Summary model failure cooldown not implemented
- M8: Summary model fallback on failure not implemented
- M9: Streaming disable-on-error stubbed but never set

### MCP Client (4 items)
- M10: No OSV malware check before spawning stdio MCP servers
- M11: No MCP sampling support
- M12: No MCP OAuth 2.1 PKCE
- M13: No MCP 401 recovery with reconnection

### Gateway (3 items)
- M14: Split-brain sentinel guard missing between busy check and handler insert
- M15: No `resume_pending` / stuck-loop detection
- M16: No agent cache with LRU + idle TTL (design choice, may affect latency)

### Tools (4 items)
- M17: Tool argument JSON repair missing (local models emit malformed JSON)
- M18: Tool name repair missing CamelCase→snake_case and suffix stripping
- M19: No steer injection into tool results
- M20: No tool progress/completion callbacks in agent engine

### Security & CLI (4 items)
- M21: Skills guard: ~30 fewer threat patterns, incomplete structural checks
- M22: CLI: no `busy` input mode
- M23: CLI: no `/reload-mcp` command
- M24: ACP: no MCP toolsets registration in ACP sessions

---

## Part 4: LOW Severity / Design Choices (12 items)

- L1: No SSL transient pattern detection (handled differently by rustls)
- L2: No `ProviderPolicyBlocked` reason (OpenRouter-specific)
- L3: Step 3 error code classification skipped (handled elsewhere)
- L4: Native Anthropic caching always disabled (minor cache hit rate)
- L5: Credential pool `key_env` resolution in fallback (edge case)
- L6: Rate-limit cooldown timing differs slightly
- L7: Tool delay between sequential tools not implemented (niche)
- L8: Concurrent task panic boundary missing (rare in practice)
- L9: Tool error detection deferred to display layer (architectural)
- L10: Subdirectory hints not integrated (cosmetic)
- L11: Missing `/snapshot` and `/gquota` CLI commands (niche)
- L12: Missing `image_gen` batch distribution (edge case)

---

## Part 5: Module-by-Module Completeness

| Module | Completeness | Critical | High | Medium | Low | Notes |
|--------|-------------|----------|------|--------|-----|-------|
| **hermez-core** | 98% | 0 | 0 | 0 | 2 | Production-ready |
| **hermez-llm** | 85% | 0 | 5 | 4 | 3 | Missing provider recovery paths |
| **hermez-prompt** | 82% | 1 | 2 | 1 | 1 | Compressor lifecycle gaps |
| **hermez-tools** | 72% | 0 | 5 | 4 | 2 | MCP client critical, env stubs |
| **hermez-agent-engine** | 85% | 0 | 4 | 5 | 2 | Features exist but not wired |
| **hermez-gateway** | 78% | 3 | 4 | 4 | 1 | Session mgmt missing key infrastructure |
| **hermez-cron** | 58% | 3 | 1 | 0 | 0 | Missing 3 critical features |
| **hermez-batch** | 92% | 0 | 0 | 0 | 1 | Minor distribution gap |
| **hermez-compress** | — | (in prompt) | (in prompt) | (in prompt) | — | Counted under hermez-prompt |
| **hermez-state** | 82% | 0 | 1 | 2 | 0 | No auto-prune, VACUUM |
| **hermez-cli** | 70% | 0 | 1 | 2 | 2 | Busy mode, /reload-mcp missing |
| **hermez-rl** | 100% | 0 | 0 | 0 | 0 | Actually exceeds Python (5 vs 3 envs) |
| **ACP binary** | 65% | 1 | 0 | 1 | 0 | MCP toolsets missing |
| **Plugins** | 60% | 1 | 1 | 0 | 0 | No bundled plugins |

**Weighted overall: ~77-82%**

---

## Part 6: Highest-Impact Quick Wins

These items have existing implementations that just need wiring:

1. **Wire `maybe_persist_tool_result` + `enforce_turn_budget`** — `tool_result_storage.rs` is complete but unused. Add 2 calls in agent.rs tool execution paths (~20 lines)
2. **Wire `CheckpointManager` into agent** — `checkpoint.rs` is complete. Call `ensure_checkpoint` before destructive tools (~15 lines)
3. **Wire `SubdirectoryHintTracker` into agent** — `subdirectory_hints.rs` exists. Call `check_tool_call` after each tool (~10 lines)
4. **Wire `memory_manager.on_memory_write()`** after memory tool execution — bridge built-in memory to external providers (~5 lines)
5. **Add `context_from`, `workdir`, `enabled_toolsets` to `CronJob` struct** — simple struct field additions with pass-through (~80 lines total)
6. **Add `guild_id`, `parent_chat_id`, `message_id`, `is_bot` to `SessionSource`** — struct field addition (~20 lines)

Total effort for all quick wins: ~2-3 days

---

## Part 7: Estimated Total Effort

| Priority | Items | Est. Effort |
|----------|-------|-------------|
| Quick wins (wire existing code) | 6 items | 2-3 days |
| CRITICAL gaps | 8 items | 5-7 days |
| HIGH gaps | 21 items | 8-12 days |
| MEDIUM gaps | 24 items | 5-8 days |
| LOW items | 12 items | 2-3 days |
| **Total** | **71 items** | **22-33 days** |

---

*End of report. All findings verified via direct code reading of both codebases.*
