# Alignment Plan — hermez-ai → hermes-agent (Python)

> **Based on**: COMPREHENSIVE_ALIGNMENT_FINAL.md (2026-04-25)
> **Total gaps**: 71 items across 12 crates
> **Estimated total effort**: 22-33 working days

---

## Phase 0: Quick Wins — Wire Existing Dead Code (2-3 days)

These features have complete implementations but are annotated `#[allow(dead_code)]` and never called from the agent loop. Just needs glue code.

### QW-1: Wire tool result persistence into agent loop

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Add calls to `maybe_persist_tool_result()` and `enforce_turn_budget()` after each tool result in both sequential and concurrent execution paths
- **Where**: After `execute_tool_call()` returns result (line ~2150), after concurrent results collected (line ~2600)
- **Existing code**: `crates/hermez-tools/src/tool_result_storage.rs` (lines 150-253, complete, dead code)
- **Python ref**: `run_agent.py:9057,9109,8694,8721`
- **Est**: 0.5 day

### QW-2: Wire checkpoint manager into agent loop

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Call `CheckpointManager::ensure_checkpoint()` before `write_file`, `patch`, and destructive `terminal` commands
- **Where**: In `execute_tool_calls_sequential` (~line 2373) and `execute_tool_calls_concurrent` (~line 2471), before tool dispatch
- **Existing code**: `crates/hermez-tools/src/checkpoint.rs` (complete, dead code)
- **Python ref**: `run_agent.py:8466-8485, 8818-8839`
- **Est**: 0.5 day

### QW-3: Wire subdirectory hints into agent loop

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Call `SubdirectoryHintTracker::check_tool_call()` after each tool result
- **Where**: After tool result is collected in both sequential and concurrent paths
- **Existing code**: `crates/hermez-prompt/src/subdirectory_hints.rs` (complete, dead code)
- **Python ref**: `run_agent.py:8701-8703, 9065-9067`
- **Est**: 0.25 day

### QW-4: Wire memory manager write bridge

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: After built-in `memory` tool writes (actions "add" or "replace"), call `self.memory_manager.on_memory_write()`
- **Where**: In `execute_tool_call()` after tool dispatch, check if tool_name == "memory" and result indicates write, then notify
- **Existing code**: `crates/hermez-agent-engine/src/memory_manager.rs` (has `on_memory_write` stub)
- **Python ref**: `run_agent.py:8367-8380, 8883-8895`
- **Est**: 0.25 day

### QW-5: Wire streaming-disable on error

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (line ~100: `disable_streaming` field)
- **What**: When a streaming API call fails with a "stream not supported" error, set `self.disable_streaming = true`
- **Where**: In the error handling block after `call_llm_stream` fails (~line 880), check error message for stream-unsupported indicators
- **Python ref**: `run_agent.py:~6554`
- **Est**: 0.25 day

### QW-6: Add concurrent activity heartbeat

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: During concurrent tool execution, periodically update activity callback with running tool names
- **Where**: In `execute_tool_calls_concurrent()`, add tokio::select between task completion and periodic timer (30s)
- **Python ref**: `run_agent.py:8595-8635`
- **Est**: 0.5 day

---

## Phase 1: CRITICAL Gaps — Cron & Compressor (3-4 days)

### CR-1: Add `context_from` field to CronJob

- **Files**: `crates/hermez-cron/src/jobs.rs` (CronJob struct, ~line 73)
- **What**: Add `context_from: Option<Vec<String>>` field. In scheduler, before job execution, resolve referenced job IDs and prepend their output to the agent's conversation context.
- **New code needed**: 
  - Struct field addition (5 lines)
  - `resolve_context_from()` method in scheduler that reads prior job outputs (~40 lines)
  - Silent skip handling when context_from job has no output yet (~30 lines)
- **Python ref**: `cron/jobs.py` + `cron/scheduler.py`, commit `5ac53659`
- **Est**: 1 day

### CR-2: Add `workdir` field to CronJob

- **Files**: `crates/hermez-cron/src/jobs.rs` (CronJob struct)
- **What**: Add `workdir: Option<String>` field. Validate as absolute path. Pass to agent config.
- **New code needed**: Struct field + validation + pass-through (~30 lines)
- **Python ref**: `cron/jobs.py`, commit `852c7f3b`
- **Est**: 0.25 day

### CR-3: Add `enabled_toolsets` field to CronJob

- **Files**: `crates/hermez-cron/src/jobs.rs` (CronJob struct)
- **What**: Add `enabled_toolsets: Option<Vec<String>>` field. Pass to agent config to restrict tools per job.
- **New code needed**: Struct field + pass-through (~20 lines)
- **Python ref**: `cron/jobs.py`, commits `0086fd89`, `8b79acb8`, `ef5eaf8d`
- **Est**: 0.25 day

### CR-4: Fix compressor token budget recalculation on model switch

- **Files**: `crates/hermez-prompt/src/context_engine.rs`, `context_compressor.rs`, `crates/hermez-agent-engine/src/agent/control.rs`
- **What**: Add `update_model()` method to `ContextEngine` trait. Implement in `ContextCompressor` to recalculate `context_length`, `threshold_tokens`, `tail_token_budget`, `max_summary_tokens`. Call from `switch_model()` and `restore_primary_runtime()`.
- **New code needed**:
  - Trait method definition (~10 lines)
  - ContextCompressor implementation (~30 lines) 
  - Two call sites in agent (~10 lines)
- **Python ref**: `context_compressor.py:301-327`, `run_agent.py:2143-2160, 6873-6886`
- **Est**: 1 day

---

## Phase 2: CRITICAL Gaps — Gateway (3-4 days)

### GW-1: Add SessionSource Discord fields

- **Files**: `crates/hermez-gateway/src/session.rs` (SessionSource struct, ~line 57)
- **What**: Add `guild_id: Option<String>`, `parent_chat_id: Option<String>`, `message_id: Option<String>`, `is_bot: Option<bool>` fields. Populate in Discord adapter message handler.
- **New code needed**: Struct fields (~10 lines), Discord adapter population (~20 lines)
- **Python ref**: `session.py:90-93`, `discord_bot.py`
- **Est**: 0.5 day

### GW-2: Implement Discord tool split

- **Files**: `crates/hermez-gateway/src/platforms/discord.rs`, `session.rs`
- **What**: Split `discord_server` into `discord` + `discord_admin` tools. Make opt-in and Discord-only. Conditionally inject Discord IDs block into system prompt when tools are loaded. Gate on `DISCORD_BOT_TOKEN` being set.
- **New code needed**: 
  - `_discord_tools_loaded()` check function (~30 lines)
  - Conditional Discord ID injection in `build_session_context_prompt()` (~40 lines)
  - Opt-in tool registration in Discord adapter (~20 lines)
- **Python ref**: `session.py:205-227, 322-334`, `discord_bot.py`, commits `81987f03`, `6ed37e0f`
- **Est**: 1 day

### GW-3: Implement drain/shutdown tool subprocess kill

- **Files**: `crates/hermez-gateway/src/runner.rs` (stop method, ~line 1555)
- **What**: Before adapter disconnect, interrupt active agents, drain with timeout, kill tool subprocesses, fire session finalize hooks, close session DB. Add `_kill_tool_subprocesses()` function.
- **New code needed**: 
  - `_kill_tool_subprocesses()` function (~50 lines)
  - Drain timeout logic in `stop()` (~30 lines)
  - Session finalize hook invocation (~20 lines)
- **Python ref**: `run.py:2599-2745`
- **Est**: 1 day

### GW-4: Implement WhatsApp identity canonicalization

- **Files**: New file `crates/hermez-gateway/src/whatsapp_identity.rs` + modify `session.rs`
- **What**: Create `canonical_whatsapp_identifier()` that strips JID/LID/device syntax and resolves LID↔phone aliases from `lid-mapping-*.json` files. Call from `build_session_key()` for WhatsApp DM and group sessions.
- **New code needed**: 
  - New module with identity normalization (~150 lines)
  - Session key generation calls (~10 lines)
- **Python ref**: `whatsapp_identity.py`, `session.py:594-610`
- **Est**: 1 day

---

## Phase 3: HIGH Severity — Agent Engine & Context (3-4 days)

### AE-1: Fix memory context injection (API-time only)

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (~line 470-494)
- **What**: Move memory injection from `messages` list (which persists to session DB) to the API-call-time message assembly in `call_llm`/`call_llm_stream`. Inject into `api_messages` rather than `messages`.
- **New code needed**: Move injection block from loop body to `call_llm`/`call_llm_stream` methods (~30 lines)
- **Python ref**: `run_agent.py:9785-9801`
- **Est**: 0.5 day

### AE-2: Fix memory injection to target current-turn user message

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Instead of finding "first user message in history", track `current_turn_user_idx` like Python does. Inject memory into that specific message.
- **New code needed**: Track index variable (~5 lines), use it for injection (~10 lines)
- **Python ref**: `run_agent.py:9439`
- **Est**: 0.25 day

### AE-3: Add last user message tail protection in compressor

- **Files**: `crates/hermez-prompt/src/context_compressor.rs` (`find_tail_cut_by_tokens`, ~line 588)
- **What**: Implement `ensure_last_user_message_in_tail()` — walk backward from end to find the last user-role message, guarantee it's in the protected tail so the active task survives compression.
- **New code needed**: `ensure_last_user_message_in_tail()` function (~40 lines)
- **Python ref**: `context_compressor.py:1007-1052`
- **Est**: 0.5 day

### AE-4: Add memory flush before compression

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (compression trigger, ~line 798)
- **What**: Before calling `engine.compress()`, call `self.memory_manager.on_pre_compress()` and run `flush_memories()` equivalent to let the model save key facts before they're lost.
- **New code needed**: Pre-compression flush calls (~30 lines)
- **Python ref**: `run_agent.py:8179-8187`
- **Est**: 0.5 day

### AE-5: Fix anti-thrashing tracking (quiet_mode guard bug)

- **Files**: `crates/hermez-prompt/src/context_compressor.rs` (~line 400-407)
- **What**: Move `ineffective_compression_count` update OUTSIDE the `!quiet_mode` guard so it always tracks. Keep logging inside the guard.
- **New code needed**: Reorder 5 lines of code
- **Python ref**: `context_compressor.py:1288-1294`
- **Est**: 0.25 day

### AE-6: Add Active Task section to summary template

- **Files**: `crates/hermez-prompt/src/context_compressor.rs` (~line 744-782)
- **What**: Add `## Active Task` as the first section in the summary template prompt, instructing the summarizer to copy the user's most recent request verbatim.
- **New code needed**: Template string addition (~10 lines)
- **Python ref**: `context_compressor.py:701-756`
- **Est**: 0.25 day

### AE-7: Wire Feishu doc/drive tools as composite

- **Files**: `crates/hermez-gateway/src/platforms/feishu_comment.rs`
- **What**: When handling a drive comment event, create agent with `enabled_toolsets=["feishu_doc", "feishu_drive"]` composite registration instead of default toolsets.
- **New code needed**: Toolset configuration in comment handler (~20 lines)
- **Python ref**: `feishu_comment.py:1086`
- **Est**: 0.5 day

---

## Phase 4: HIGH Severity — LLM Providers (3-4 days)

### LLM-1: Add separate streaming stale-timeout with SSE awareness

- **Files**: `crates/hermez-agent-engine/src/agent/utils.rs`, `agent.rs`
- **What**: Implement separate stale-call timeout for streaming. Add SSE keep-alive ping detection (distinguish pings from real data via `last_chunk_time`). Scale timeout for large contexts (>100K tokens: ≥300s, >50K: ≥240s).
- **New code needed**: 
  - `streaming_stale_timeout()` function (~40 lines)
  - Integration into `call_llm_stream` (~20 lines)
- **Python ref**: `run_agent.py:6551+`
- **Est**: 1 day

### LLM-2: Implement OAuth refresh for Codex/Nous/Copilot

- **Files**: `crates/hermez-llm/src/credential_pool.rs`, `crates/hermez-agent-engine/src/failover.rs`
- **What**: Add separate refresh paths for Codex (407-specific endpoint), Nous (agent key refresh protocol), and Copilot (credential refresh). Currently only Anthropic is handled.
- **New code needed**: 
  - Codex refresh function (~50 lines)
  - Nous refresh function (~40 lines)
  - Copilot refresh function (~30 lines)
  - Failover dispatch changes (~20 lines)
- **Python ref**: `run_agent.py:10870-10945`
- **Est**: 1.5 days

### LLM-3: Add stale-call per-provider config from config.yaml

- **Files**: `crates/hermez-agent-engine/src/agent/utils.rs`
- **What**: Read stale timeout from `config.yaml` per-provider/per-model, falling back to env var, defaulting to 300s.
- **New code needed**: Config lookup in timeout functions (~30 lines)
- **Python ref**: `run_agent.py:2547`
- **Est**: 0.5 day

### LLM-4: Add stream delta Ollama index/id fix

- **Files**: `crates/hermez-llm/src/client.rs` (streaming handler, ~line 373-500)
- **What**: Track `last_id_at_idx` and `active_slot_by_idx` to detect tool calls reusing index 0 but with different IDs. Redirect to fresh slots when detected.
- **New code needed**: Slot tracking struct + logic (~60 lines)
- **Python ref**: `run_agent.py:6118`
- **Est**: 0.5 day

### LLM-5: Add retry recovery strategies

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Add: (a) invalid tool name recovery — send error feedback to model for self-correction (max 3), (b) partial stream recovery — use delivered content if connection dies mid-stream, (c) thinking prefill retries — on thinking-only responses, append as prefill.
- **New code needed**: Three recovery blocks in error handling (~150 lines total)
- **Python ref**: `run_agent.py` multiple locations
- **Est**: 1 day

---

## Phase 5: HIGH Severity — MCP Client (3-4 days)

### MCP-1: Implement MCP HTTP transport

- **Files**: `crates/hermez-tools/src/mcp_client/` (new file: `http_transport.rs`)
- **What**: Full Streamable HTTP transport with SSE support. POST initialize + GET SSE endpoint. Header seeding. Error handling.
- **New code needed**: ~300 lines
- **Python ref**: `tools/mcp_tool.py` HTTP transport section
- **Est**: 1.5 days

### MCP-2: Implement MCP schema normalization

- **Files**: `crates/hermez-tools/src/mcp_client/` (new file: `schema_normalize.rs`)
- **What**: Rewrite `$defs` refs to `$defs` in input schemas. Strip integer/number/boolean enums (Gemini rejects them). Coerce stringified arrays/objects in tool args.
- **New code needed**: ~150 lines
- **Python ref**: `tools/mcp_tool.py` `_normalize_mcp_input_schema()`
- **Est**: 0.5 day

### MCP-3: Implement MCP auto-reconnect

- **Files**: `crates/hermez-tools/src/mcp_client/server.rs`
- **What**: Add exponential backoff retry on connect failure (3 attempts). Add session expiry detection and one automatic retry. Max 5 retries with 60s backoff cap.
- **New code needed**: Reconnect logic (~100 lines)
- **Python ref**: `tools/mcp_tool.py`, commit `e87a2100`
- **Est**: 0.5 day

### MCP-4: Add MCP stderr routing to log file

- **Files**: `crates/hermez-tools/src/mcp_client/server.rs` (~line 58)
- **What**: Route MCP stdio subprocess stderr to `~/.hermez/logs/mcp-stderr-{server_name}.log` instead of piping to parent process stderr which floods the user terminal.
- **New code needed**: File creation + stderr redirection (~40 lines)
- **Python ref**: `tools/mcp_tool.py`, commit `379b2273`
- **Est**: 0.25 day

### MCP-5: Add MCP auth stripping on cross-origin redirect

- **Files**: `crates/hermez-tools/src/mcp_client/` (HTTP transport)
- **What**: Strip auth headers when HTTP transport follows a cross-origin redirect. Prevents credential leaks to different hosts.
- **New code needed**: Redirect detection + header stripping (~30 lines)
- **Python ref**: `tools/mcp_tool.py`, commit `8c2732a9`
- **Est**: 0.25 day

---

## Phase 6: HIGH Severity — Security & Gateway Remaining (2-3 days)

### SG-1: Implement smart LLM-based approval

- **Files**: `crates/hermez-tools/src/approval.rs` (~line 294-316)
- **What**: When `ApprovalMode::Smart`, call auxiliary LLM to evaluate command risk level. Fall back to pattern matching if LLM unavailable.
- **New code needed**: LLM call integration in `evaluate_command()` (~80 lines)
- **Python ref**: `tools/approval.py` smart approval section
- **Est**: 1 day

### SG-2: Add gateway blocking flow

- **Files**: `crates/hermez-gateway/src/runner.rs`, `crates/hermez-tools/src/approval.rs`
- **What**: Per-entry `ApprovalEntry` queues for blocking. Activity heartbeat during wait. Cron mode config (`approvals.cron_mode`).
- **New code needed**: Queue struct + heartbeat (~100 lines)
- **Python ref**: `tools/approval.py` gateway section
- **Est**: 1 day

### SG-3: Add URL query/userinfo redaction

- **Files**: `crates/hermez-core/src/redact.rs`
- **What**: Add `_redact_url_query_params()` — detect and strip `?access_token=...&code=...` from URLs. Add `_redact_url_userinfo()` — strip `user:password@` from HTTP/WS/FTP URLs.
- **New code needed**: Two regex-based redaction functions (~50 lines)
- **Python ref**: `agent/redact.py` URL sections
- **Est**: 0.25 day

### SG-4: Add session expiry watcher with finalize hooks

- **Files**: `crates/hermez-gateway/src/runner.rs`
- **What**: Background task every 5 minutes: scan for expired sessions, flush memories, fire `on_session_finalize` plugin hook, clean agent resources, evict from cache, set `memory_flushed` flag.
- **New code needed**: Watcher task (~150 lines)
- **Python ref**: `run.py:2291-2408`
- **Est**: 0.5 day

### SG-5: Implement SSE streaming callback wiring

- **Files**: `crates/hermez-gateway/src/platforms/api_server.rs` (~line 1315), `crates/hermez-agent-engine/src/agent.rs`
- **What**: Remove the TODO at line 1315. Wire `stream_delta_callback` from API server through to agent engine. Replace post-hoc character chunking with true streaming.
- **New code needed**: Callback wiring (~50 lines), remove chunking hack (~10 lines)
- **Python ref**: `api_server.py:708,748,921-970`
- **Est**: 1 day

---

## Phase 7: MEDIUM Severity — Tools & Quality (3-4 days)

### Tool-1: Add tool argument JSON repair

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (~line 1948-1958)
- **What**: Multi-pass JSON repair: empty→`{}`, `None`→`{}`, re-serialize via `serde_json`, trailing comma removal, bracket balancing (bounded 50 iters), escape control chars, last resort `{}`.
- **New code needed**: `repair_tool_call_arguments()` function (~80 lines)
- **Python ref**: `run_agent.py:547-641`
- **Est**: 0.5 day

### Tool-2: Add tool name repair steps 3 & 4

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (~line 1866-1901)
- **What**: Add CamelCase→snake_case conversion (step 3) and `_tool`/`-tool`/`tool` suffix stripping (step 4, up to 2x). Critical for Claude models that emit `TodoTool_tool`.
- **New code needed**: Two additional repair steps (~30 lines)
- **Python ref**: `run_agent.py:4656-4726`
- **Est**: 0.25 day

### Tool-3: Add steer injection into tool results

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Accumulate pending steer text from `/steer` command. After each tool result, append steer to last tool message with `User guidance:` marker. Consume atomically.
- **New code needed**: Pending steer state + injection (~60 lines)
- **Python ref**: `run_agent.py:8715, 9080, 8728, 9115`
- **Est**: 0.5 day

### Tool-4: Add tool progress/completion callbacks

- **Files**: `crates/hermez-agent-engine/src/agent.rs`
- **What**: Add `tool_progress_callback` and `tool_complete_callback` to agent config. Fire before/after each tool in both sequential and concurrent paths.
- **New code needed**: Callback fields + fire points (~40 lines)
- **Python ref**: `run_agent.py:8502-8515, 8660-8692`
- **Est**: 0.5 day

### Tool-5: Add `approve_always` persistence in concurrent path

- **Files**: `crates/hermez-agent-engine/src/agent.rs` (~line 2647-2651)
- **What**: In concurrent post-processing, when `choice == "approve_always"`, also save to `approval_allowlist.json` on disk (currently only in-memory).
- **New code needed**: File persistence logic from sequential path (~20 lines)
- **Python ref**: `run_agent.py:2088-2116`
- **Est**: 0.25 day

### GW-6: Add split-brain sentinel guard

- **Files**: `crates/hermez-gateway/src/runner.rs` (~line 277)
- **What**: Insert `_AGENT_PENDING_SENTINEL` placeholder into `running_sessions` map BEFORE awaiting handler lock. Check for sentinel on second message arrival.
- **New code needed**: Sentinel marker + pre-insert + sentinel check (~30 lines)
- **Python ref**: `run.py:315`
- **Est**: 0.25 day

### GW-7: Add resume_pending / stuck-loop detection

- **Files**: `crates/hermez-gateway/src/runner.rs`, `session.rs`
- **What**: Add `resume_pending` flag to SessionEntry. Set on drain timeout interrupt, clear on successful next turn. Track restart failure counts. Auto-suspend sessions stuck across 3+ restarts.
- **New code needed**: Fields + tracking logic (~120 lines)
- **Python ref**: `session.py:459-998`
- **Est**: 0.5 day

### State-1: Add auto-prune + VACUUM at startup

- **Files**: `crates/hermez-state/src/session_db.rs` (~line 60-75)
- **What**: Call `prune_sessions()` during `SessionDB::open()`. Run `VACUUM` after pruning to reclaim disk space.
- **New code needed**: Two function calls in open (~10 lines)
- **Python ref**: `session.py` startup logic, commit `b8663813`
- **Est**: 0.25 day

---

## Phase 8: MEDIUM Severity — CLI & ACP (1-2 days)

### CLI-1: Add busy input mode

- **Files**: `crates/hermez-cli/src/app.rs`, `slash_commands.rs`
- **What**: Add `busy` slash command with `queue|interrupt|status` subcommands. Block input during `/compress` and other long operations.
- **New code needed**: Busy state management (~80 lines), command handler (~40 lines)
- **Python ref**: `commands.py:busy` section, commits `fd3864d8`, `1dcf79a8`
- **Est**: 0.5 day

### CLI-2: Add /reload-mcp command

- **Files**: `crates/hermez-cli/src/slash_commands.rs`
- **What**: Add command to reload MCP server connections without restarting the agent.
- **New code needed**: Command handler (~40 lines)
- **Python ref**: `commands.py:/reload-mcp`
- **Est**: 0.25 day

### ACP-1: Add MCP toolsets to ACP sessions

- **Files**: `src/hermez_acp/server.rs`
- **What**: Implement `_register_session_mcp_servers()` — map ACP-provided MCP servers to session tool registrations.
- **New code needed**: MCP registration in ACP session init (~80 lines)
- **Python ref**: `acp_adapter/server.py` `_register_session_mcp_servers`
- **Est**: 0.5 day

### Skills-1: Complete skills guard structural checks

- **Files**: `crates/hermez-tools/src/skills_guard.rs` (~line 197-215)
- **What**: Add file count limit, total size limit, single file size limit, binary file detection, executable permission check.
- **New code needed**: 5 additional structural checks (~60 lines)
- **Python ref**: `tools/skills_guard.py` structural checks
- **Est**: 0.25 day

---

## Phase 9: LOW Priority / Polish (2-3 days, optional)

12 items. Can be deferred or done incrementally:
- L1-L3: Error classifier refinements (SSL, ProviderPolicyBlocked, Step 3)
- L4-L6: Credential pool edge cases (native cache, key_env, rate-limit cooldown)
- L7-L9: Tool execution polish (delay, panic boundary, error detection)
- L10-L12: CLI commands (/snapshot, /gquota, subdirectory hints)

Each item is 0.1-0.25 day.

---

## Execution Sequence

```
Phase 0 (2-3d):  Quick Wins ── wire dead code
    │
Phase 1 (3-4d):  CRITICAL ── Cron fields, compressor model switch
    │
Phase 2 (3-4d):  CRITICAL ── Gateway SessionSource, Discord, drain, WhatsApp
    │
Phase 3 (3-4d):  HIGH ── Compressor/memory/Feishu fixes
    │
Phase 4 (3-4d):  HIGH ── LLM providers (parallel with Phase 3)
    │
Phase 5 (3-4d):  HIGH ── MCP client (can start after Phase 0)
    │
Phase 6 (2-3d):  HIGH ── Security & Gateway remaining
    │
Phase 7 (3-4d):  MEDIUM ── Tools & quality
    │
Phase 8 (1-2d):  MEDIUM ── CLI & ACP
    │
Phase 9 (2-3d):  LOW ── Polish (optional)
```

Phases 3 and 4 can run in parallel. Phase 5 can start after Phase 0.

### Critical path: 
`Phase 0 → Phase 1 → Phase 2 → Phase 6 → Phase 7 → Phase 8`
(Total: 14-20 days on critical path)

### Parallelizable: 
Phase 3, 4, 5 can run alongside Phase 1-2 (3-4 days saved)

---

## Verification Plan

For each phase, run:
1. `cargo build --workspace` — must compile cleanly
2. `cargo test --workspace` — all ~2,039 tests must pass
3. `cargo clippy --workspace` — no new warnings introduced
4. For gateway changes: manual smoke test with at least one platform adapter
5. For provider changes: integration test with credential pool

---

*End of alignment plan.*
