# Hermes-RS Architecture

## 1. System Layer Architecture (5-Tier)

```
+------------------------------------------------------------------------+
|                         Binary Targets (Tier 5)                        |
|                                                                        |
|  ┌──────────────┐  ┌─────────────────┐  ┌──────────────────────────┐   |
|  │   hermes     │  │  hermez-agent   │  │      hermez-acp          │   |
|  │  (main CLI)  │  │ (standalone)    │  │ (JSON-RPC IDE server)    │   |
|  │  31 cmds     │  │ single-purpose  │  │ 13 methods, sessions     │   |
|  └──────┬───────┘  └────────┬────────┘  └──────────┬───────────────┘   |
|         │                   │                       │                   |
+---------┼───────────────────┼───────────────────────┼-------------------+
|         │                   │                       │                   |
|  ┌──────▼──────────────────▼───────────────────────▼──────────────┐    |
|  │                    CLI / Adapter Layer (Tier 4)                 │    |
|  │                                                                 │    |
|  │  hermez-cli          hermez-compress      hermez-batch          │    |
|  │  31 subcommands      Context compression  Batch processing      │    |
|  │  TUI, config         4-stage algorithm    Multi-session         │    |
|  │  backup, cron, etc.                         │                   │    |
|  │                                             │                   │    |
|  │  hermez-cron         hermez-gateway         │                   │    |
|  │  Scheduler           19 platform adapters   │                   │    |
|  │  Job mgmt            Telegram/Discord/      │                   │    |
|  │                      Weixin/Feishu/...      │                   │    |
|  └──────────────────────────┬──────────────────┘                    |
|                             │                                       |
+-----------------------------┼---------------------------------------+
|                             │                                       |
|  ┌──────────────────────────▼──────────────────────────────┐        |
|  │              Agent Engine Layer (Tier 3)                 │        |
|  │                                                          │        |
|  │  ┌─────────────────────────────────────────────────┐     │        |
|  │  │              AIAgent (core loop)                 │     │        |
|  │  │  run_conversation() → tool dispatch → respond    │     │        |
|  │  └──┬────┬────┬────┬────┬────┬────┬────┬──────────┘     │        |
|  │     │    │    │    │    │    │    │    │                │        |
|  │  ┌──▼┐┌─▼──┐│┌───▼┐│┌───▼──┐│┌──▼──┐│┌───▼─────────┐  │        |
|  │  │Mem││Fail│││Sub │││Smart │││Title│││Trajectory   │  │        |
|  │  │ory││over│││agent│││Route │││Gen  │││Saver        │  │        |
|  │  │Mgr││Chain│││Mgr  │││(cheap│││     │││             │  │        |
|  │  │   ││     │││     │││/strong)││     │││             │  │        |
|  │  └───┘└─────┘│└─────┘│└──────┘│└─────┘│└─────────────┘  │        |
|  │     │         │       │        │       │                  │        |
|  │  ┌──▼─────────▼───────▼────────▼───────▼──────────────┐  │        |
|  │  │              MessageLoop                            │  │        |
|  │  │  PlatformMessage bridge → AIAgent → MessageResult   │  │        |
|  │  └─────────────────────────────────────────────────────┘  │        |
|  │                                                            │        |
|  │  self_evolution  │  review_agent  │  skill_commands        │        |
|  │  Self-improve    │  Quality eval  │  Skill scanning        │        |
|  └──────────────────┬─────────────────────────────────────────┘        |
|                     │                                                  |
+─────────────────────┼──────────────────────────────────────────────────+
|                     │                                                  |
|  ┌──────────────────▼────────────────────────────────────────────┐    |
|  │                  Service Layer (Tier 2)                        │    |
|  │                                                                │    |
|  │  ┌─────────────────────┐  ┌──────────────────────────────┐    │    |
|  │  │    hermez-tools     │  │       hermez-prompt          │    │    |
|  │  │                     │  │                              │    │    |
|  │  │  ToolRegistry       │  │  build_system_prompt         │    │    |
|  │  │  register_all_tools │  │  ContextCompressor (4-stage) │    │    |
|  │  │                     │  │  apply_anthropic_cache_ctrl  │    │    |
|  │  │  55+ tool modules:  │  │  sanitize_context_content    │    │    |
|  │  │  file_ops, terminal,│  │  build_skills_system_prompt  │    │    |
|  │  │  web, browser,      │  │  load_soul_md                │    │    |
|  │  │  code_exec, delegate,│ │  subdirectory_hints          │    │    |
|  │  │  mcp_client, memory,│  │  TOOL_USE_ENFORCEMENT_GUID.  │    │    |
|  │  │  tts, voice, vision,│  │  MEMORY_GUIDANCE             │    │    |
|  │  │  skills, cron_tools,│  │                              │    │    |
|  │  │  rl_training, moa   │  │  9 modules                   │    │    |
|  │  │                     │  │                              │    │    |
|  │  │  Toolsets:          │  │                              │    │    |
|  │  │  hermez-cli, web,   │  │                              │    │    |
|  │  │  terminal, file,    │  │                              │    │    |
|  │  │  vision, browser,   │  │                              │    │    |
|  │  │  skills, delegate,  │  │                              │    │    |
|  │  │  cron, voice, tts,  │  │                              │    │    |
|  │  │  ha, memory, code,  │  │                              │    │    |
|  │  │  planning, clarify  │  │                              │    │    |
|  │  │                     │  │                              │    │    |
|  │  │  Env backends:      │  │                              │    │    |
|  │  │  local, docker, ssh,│  │                              │    │    |
|  │  │  daytona, singul.,  │  │                              │    │    |
|  │  │  modal              │  │                              │    │    |
|  │  └─────────────────────┘  └──────────────────────────────┘    │    |
|  └──────────────────────────┬─────────────────────────────────────┘    |
|                             │                                          |
+─────────────────────────────┼──────────────────────────────────────────+
|                             │                                          |
|  ┌──────────────────────────▼──────────────────────────────────┐      |
|  │              Infrastructure Layer (Tier 1)                    │      |
|  │                                                              │      |
|  │  ┌───────────────────┐  ┌─────────────────┐  ┌───────────┐  │      |
|  │  │   hermez-llm      │  │  hermez-state   │  │hermez-gate│  │      |
|  │  │                   │  │                 │  │ (platform)│  │      |
|  │  │  call_llm()       │  │  SessionDB      │  │           │  │      |
|  │  │  ProviderType(11) │  │  SQLite+WAL     │  │ Platform  │  │      |
|  │  │  Anthropic client │  │  FTS5 search    │  │ enum(19)  │  │      |
|  │  │  Codex client     │  │  Session model  │  │ session   │  │      |
|  │  │  Auxiliary 5-tier │  │  InsightsEngine │  │ store     │  │      |
|  │  │  CredentialPool   │  │  Token/cost     │  │ hash PII  │  │      |
|  │  │  Error classifier │  │  tracking       │  │ dedup     │  │      |
|  │  │  Model metadata   │  │                 │  │           │  │      |
|  │  │  Rate limiting    │  │                 │  │ 5 platform│  │      |
|  │  │  Token estimate   │  │                 │  │ adapters  │  │      |
|  │  │  Reasoning extract│  │                 │  │           │  │      |
|  │  │  Retry/backoff    │  │                 │  │           │  │      |
|  │  │                   │  │                 │  │           │  │      |
|  │  │  15 modules       │  │  5 modules      │  │ 8 modules │  │      |
|  │  └───────────────────┘  └─────────────────┘  └───────────┘  │      |
|  └──────────────────────────┬──────────────────────────────────┘      |
|                             │                                          |
+─────────────────────────────┼──────────────────────────────────────────+
|                             │                                          |
|  ┌──────────────────────────▼──────────────────────────────────┐      |
|  │              Core Layer (Tier 0)                              │      |
|  │                                                              │      |
|  │  ┌──────────────────────────────────────────────────────┐   │      |
|  │  │                   hermez-core                         │   │      |
|  │  │                                                       │   │      |
|  │  │  HermesConfig (load/save, defaults, env vars)        │   │      |
|  │  │  HermesError + ErrorCategory                          │   │      |
|  │  │  Result<T> type alias                                 │   │      |
|  │  │  Constants (HERMES_HOME, version, etc.)               │   │      |
|  │  └──────────────────────────────────────────────────────┘   │      |
|  └─────────────────────────────────────────────────────────────┘      |
|                                                                        |
+------------------------------------------------------------------------+
```

## 2. Crate Dependency Graph (DAG)

```
                         hermes (workspace root)
                    ┌──────┬───────┬───────┐
                    │      │       │       │
              hermez-cli  │  hermez-agent  hermez-acp
                    │     │       │       │
                    │     └───┬───┘       │
                    │         │           │
              ┌─────▼─────────▼───────────▼────┐
              │        hermez-agent-engine      │
              │  agent, message_loop, failover   │
              │  memory, subagent, skill cmds   │
              │  self_evolution, review_agent   │
              │  smart_routing, title, budget   │
              │  trajectory                     │
              └──┬──────┬──────────┬───────┬───┘
                 │      │          │       │
          ┌──────▼──┐ ┌─▼─────┐ ┌──▼────┐ │
          │hermes   │ │hermes │ │hermes │ │
          │-prompt  │ │-tools │ │-state │ │
          └────┬────┘ └───┬───┘ └───┬───┘ │
               │          │         │     │
          ┌────▼──────────▼─────────▼─────▼───┐
          │         hermez-llm                 │
          │  client, provider, anthropic       │
          │  codex, credential_pool,           │
          │  error_classifier, auxiliary,      │
          │  models_dev, retry, reasoning      │
          │  tool_call (10 provider parsers)   │
          └──────────────┬─────────────────────┘
                         │
          ┌──────────────▼─────────────────────┐
          │         hermez-core                 │
          │  HermesConfig, HermesError,         │
          │  Result<T>, constants               │
          └─────────────────────────────────────┘

  Additional crates (optional/default-excluded):

  hermez-gateway ──→ hermez-core
  hermez-cron      ──→ hermez-core
  hermez-compress  ──→ hermez-core
  hermez-batch     ──→ hermez-core
```

## 3. Agent Engine Internal Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        AIAgent.run_conversation()                    │
│                                                                      │
│  ┌────────────┐    ┌──────────────────────────────────────────┐     │
│  │  Messages  │───▶│          Prompt Builder                   │     │
│  │  (input)   │    │  build_system_prompt()                   │     │
│  └────────────┘    │  + soul.md identity                      │     │
│                    │  + skill prompts                         │     │
│                    │  + cache control markers                 │     │
│                    │  + injection sanitization                │     │
│                    └──────────────────┬───────────────────────┘     │
│                                     │                               │
│                    ┌────────────────▼───────────────────────┐       │
│                    │           call_llm()                    │       │
│                    │  Provider routing (11 types)            │       │
│                    │  Credential pool selection              │       │
│                    │  Provider preferences (OpenRouter)      │       │
│                    │  Failover chain on error                │       │
│                    └──────────────────┬──────────────────────┘       │
│                                     │                               │
│                    ┌────────────────▼───────────────────────┐       │
│                    │         LLM Response                   │       │
│                    └──┬──────────────────────────┬──────────┘       │
│                       │                          │                  │
│              ┌────────▼──────┐          ┌───────▼────────┐         │
│              │ Text content  │          │  Tool calls    │         │
│              │               │          │                │         │
│              │ → Memory sync │          │ → Subagent?    │         │
│              │ → Title gen   │          │   depth<=2     │         │
│              │ → Trajectory  │          │ → Tool dispatch│         │
│              │ → Budget check│          │ → Result storage│        │
│              └───────────────┘          │ → Next iteration│        │
│                                         └────────────────┘         │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                   Failover Chain (on error)                   │    │
│  │  1. Sanitize Unicode (surrogates, max 2 passes)              │    │
│  │  2. Classify error (rate_limit/auth/billing/context/etc.)   │    │
│  │  3. Credential pool rotation (401/402/429 handling)         │    │
│  │  4. Provider-specific auth refresh                          │    │
│  │  5. Strip thinking signature (one-shot)                     │    │
│  │  6. Compress context (4-stage: prune/protect/summarize)     │    │
│  │  7. Rate limit eager fallback                               │    │
│  │  8. Payload too large → compress                            │    │
│  │  9. Context overflow → compress                             │    │
│  │ 10. Non-retryable → fallback → abort                        │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                   Subagent Delegation                        │    │
│  │  Max depth: 2 │ Max concurrent: 3 │ Isolated tool subset    │    │
│  │  5 blocked tools: terminal/code_exec/browser/etc.           │    │
│  │  Independent budget + interrupt propagation                 │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                   Smart Model Routing                        │    │
│  │  Heuristic: keyword/URL/code detection                       │    │
│  │  Route to cheap model vs strong model per turn              │    │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

## 4. LLM Provider Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        call_llm()                                 │
│                                                                   │
│  ┌─────────────┐    ┌─────────────────────────────────────────┐  │
│  │ LlmRequest  │───▶│          Provider Routing                │  │
│  │ model       │    │  resolve_provider_alias()               │  │
│  │ base_url    │    │  detect_aggregator(base_url)            │  │
│  │ api_key     │    │  parse_provider()                       │  │
│  │ provider    │    │                                         │  │
│  └─────────────┘    │  11 Provider Types:                     │  │
│                     │  OpenRouter │ Nous │ Custom │ Codex     │  │
│                     │  Gemini │ Zai │ Kimi │ Minimax          │  │
│                     │  Anthropic │ OpenAI │ Unknown            │  │
│                     └──────────────────┬──────────────────────┘  │
│                                        │                          │
│              ┌─────────────────────────┼──────────────────┐      │
│              │                         │                   │      │
│     ┌────────▼───────┐      ┌──────────▼─────┐  ┌────────▼───┐  │
│     │ Anthropic      │      │ OpenAI-compat  │  │ Codex      │  │
│     │ (reqwest)      │      │ (async-openai) │  │ (responses)│  │
│     │                │      │                │  │            │  │
│     │ + cache control│      │ + extra_body   │  │ input items│  │
│     │ + thinking     │      │ + provider pref│  │ stream     │  │
│     │ + tool_use     │      │                │  │            │  │
│     └────────┬───────┘      └────────┬───────┘  └─────┬──────┘  │
│              │                       │                  │        │
│              └───────────────────────┼──────────────────┘        │
│                                      │                            │
│                     ┌────────────────▼─────────────────┐         │
│                     │        Response Processing        │         │
│                     │  extract_reasoning() (4 formats)  │         │
│                     │  parse_tool_calls (10 providers)  │         │
│                     │  token usage tracking             │         │
│                     └────────────────┬──────────────────┘         │
│                                      │                            │
│                     ┌────────────────▼─────────────────┐         │
│                     │        Error Handling              │         │
│                     │  classify_api_error()              │         │
│                     │  FailoverReason: RateLimit/Auth/  │         │
│                     │  Billing/ContextOverflow/          │         │
│                     │  PayloadTooLarge/ThinkingSig/     │         │
│                     │  Transport/Unknown                 │         │
│                     │                                   │         │
│                     │  CredentialPool:                   │         │
│                     │  select() / mark_exhausted()      │         │
│                     │  try_refresh_current()            │         │
│                     │                                   │         │
│                     │  Retry: exponential backoff       │         │
│                     │  Rate limit: token bucket         │         │
│                     └───────────────────────────────────┘         │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │              Auxiliary LLM 5-Tier Fallback                 │   │
│  │  (for compression, search, vision, side tasks)            │   │
│  │                                                           │   │
│  │  OpenRouter → Nous → Custom → Codex → API-key provider   │   │
│  └───────────────────────────────────────────────────────────┘   │
└───────────────────────────────────────────────────────────────────┘
```

## 5. Tool Registry Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                     register_all_tools()                          │
│                                                                   │
│  Registration order (startup):                                    │
│  todo → clarify → fuzzy_match → memory → approval → web →        │
│  vision → homeassistant → skills → skills_hub → file_ops →       │
│  image_gen → cron_tools → session_search → send_message →        │
│  tts → voice → process_reg → terminal → delegate →               │
│  mcp_client → rl_training → browser → code_exec → moa            │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                    ToolRegistry                            │   │
│  │  HashMap<String, ToolEntry>                                │   │
│  │  register / deregister / get / dispatch / definitions     │   │
│  │  list_tools / get_available_tools / get_handler            │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                   │
│  Toolsets (composition via includes):                             │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │  hermez-cli (33 core tools)                              │     │
│  │  web, terminal, file, vision, image, browser             │     │
│  │  skills, planning, memory, code, delegate                │     │
│  │  cron, voice, tts, ha, session_search                    │     │
│  │  clarify, send_message, rl_training, moa                 │     │
│  └─────────────────────────────────────────────────────────┘     │
│                                                                   │
│  Environment Backends (6):                                        │
│  local │ docker │ ssh │ daytona │ singularity │ modal             │
│                                                                   │
│  Tool Handler Signature:                                          │
│  Fn(Value) -> Result<String> + Send + Sync                       │
│  with: check_fn, requires_env, is_async, max_result_size_chars   │
└───────────────────────────────────────────────────────────────────┘
```

## 6. Gateway Platform Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      hermez-gateway                              │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                    Platform enum (20)                      │   │
│  │                                                           │   │
│  │  Local │ Telegram │ Discord │ Whatsapp │ Slack │ Signal   │   │
│  │  Mattermost │ Matrix │ Homeassistant │ Email │ Sms        │   │
│  │  Dingtalk │ ApiServer │ Webhook │ Feishu                  │   │
│  │  Wecom │ WecomCallback │ Weixin │ Bluebubbles             │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                  Platform Adapters (5)                     │   │
│  │  api_server  │  dingtalk  │  feishu  │  wecom  │  weixin   │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                    Session Management                      │   │
│  │  SessionSource │ SessionContext │ SessionEntry             │   │
│  │  SessionStore │ PII redaction (hash_sender/hash_chat)     │   │
│  │  Reset policy │ Stream consumer                           │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                    Supporting Modules                      │   │
│  │  Message dedup │ MCP config │ Streaming config             │   │
│  └───────────────────────────────────────────────────────────┘   │
└───────────────────────────────────────────────────────────────────┘
```

## 7. Binary Flow Comparison

```
┌─────────────────────────────────────────────────────────────────┐
│                        hermes (CLI)                              │
│                                                                  │
│  clap arg parsing → hermez-cli dispatch → 31 subcommands:       │
│  chat, setup, doctor, config, tools, skills, models, status,    │
│  sessions, backup, restore, gateway, cron, profiles, insights,  │
│  update, uninstall, completion, acp, logs, debug, dump,         │
│  auth, login, logout, memory, mcp, webhooks, whatsapp,          │
│  pairing, plugins, dashboard, version                            │
│                                                                  │
│  Chat flow: ToolRegistry → register_all_tools → AgentConfig →   │
│  AIAgent → run_conversation() → print response                  │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                     hermez-agent (standalone)                    │
│                                                                  │
│  clap: --model --max-iterations --enabled-toolsets              │
│        --disabled-toolsets --quiet --save-trajectories          │
│        --skip-context-files --skip-memory --verbose              │
│                                                                  │
│  ToolRegistry → register_all_tools → AgentConfig → AIAgent →   │
│  read stdin → run_conversation() → print response → loop        │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    hermez-acp (IDE integration)                  │
│                                                                  │
│  JSON-RPC 2.0 over stdin/stdout                                  │
│                                                                  │
│  13 methods: initialize, authenticate, newSession,              │
│  loadSession, resumeSession, cancel, forkSession, listSessions, │
│  prompt, setSessionModel, setSessionMode, setConfigOption       │
│                                                                  │
│  SessionManager (RwLock<HashMap>) → AcpServer →                │
│  run_jsonrpc() loop → stdout updates via unbounded channel      │
│                                                                  │
│  Session updates: AgentMessage, AgentThought, ToolCallStart,    │
│  ToolCallContent, AvailableCommandsUpdate                        │
└─────────────────────────────────────────────────────────────────┘
```

## 8. Python → Rust Module Mapping

| Python Module | Rust Equivalent | Status |
|---------------|----------------|--------|
| `run_agent.py:AIAgent` | `hermez-agent-engine/agent.rs:AIAgent` | Aligned |
| `config.py:load_config` | `hermez-core/config.rs:HermesConfig::load` | + env expand |
| `toolsets.py` | `hermez-tools/toolsets_def.rs` | 20+ toolsets |
| `model_tools.py` | `hermez-tools/` (55 files) | Aligned |
| `hermez_state.py` | `hermez-state/session_db.rs:SessionDB` | FTS5 |
| `cli.py` | `hermez-cli/` (31 subcommands) | Aligned |
| `_recover_with_credential_pool` | `hermez-agent-engine/failover.rs` | Aligned |
| `MemoryProvider` (ABC) | `hermez-agent-engine/memory_provider.rs` | Trait defined |
| `_extract_reasoning` | `hermez-llm/reasoning.rs` | 4 formats |
| `prompt builder` | `hermez-prompt/builder.rs` | + cache control |
| `context_compressor.py` | `hermez-prompt/context_compressor.rs` | 4-stage |
| `gateway/run.py` | `hermez-gateway/runner.rs` | 20 platforms |
| `cron/jobs.py` | `hermez-cron/` | Aligned |
| `acp_adapter/` | `hermez-acp/` | 13 methods |

## 9. Web Dashboard API

The web dashboard (`hermez-cli/src/web_server.rs`) serves a React SPA from `web/dist/` and provides REST/SSE endpoints for session management, real-time chat, and system administration.

### API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/status` | System overview (sessions, tokens, cron, disk) |
| GET | `/api/sessions` | List all sessions |
| POST | `/api/sessions` | Create new session |
| GET | `/api/sessions/:id` | Session detail + message history |
| DELETE | `/api/sessions/:id` | Delete session |
| POST | `/api/sessions/:id/rename` | Rename session |
| POST | `/api/sessions/:id/chat` | Blocking chat (returns full response) |
| POST | `/api/sessions/:id/chat-stream` | **SSE streaming chat** (delta → done) |
| GET | `/api/config` | Read `~/.hermez/config.yaml` |
| POST | `/api/config` | Write `~/.hermez/config.yaml` |
| GET | `/api/cron` | List cron jobs |
| GET | `/api/plugins` | Discover installed plugins |

### Frontend Pages

| Route | Page | Key Features |
|-------|------|--------------|
| `/` | Dashboard | Stats cards, recent sessions, "+ New Chat" |
| `/sessions` | Sessions | Searchable table, platform/model badges |
| `/sessions/:id` | Session Detail | Message history, **Markdown rendering**, inline chat, delete/rename |
| `/status` | Status | System info, platform connectivity, cron jobs |
| `/settings` | Settings | **Inline config editor** (string/number/boolean), save to YAML |
| `/plugins` | Plugins | Plugin cards with tools/hooks/technology tags |

### Streaming Architecture

```
Browser ──POST /chat-stream──► Axum SSE
                                  │
                                  ▼
                         AIAgent::set_stream_callback()
                                  │
                                  ▼
                         mpsc::channel ──► ReceiverStream
                                  │
                                  ▼
                         text/event-stream (delta / done / error)
```

## 10. Plugin Architecture

Hermes supports both **Phase-2 WASM ABI** (manual memory management) and **WebAssembly Component Model** (wit-bindgen, type-safe).

### WIT Interface (`wit/world.wit`)

```
world hermez-plugin {
    import host;      // log, get-config, invoke-tool
    export plugin;    // register, on-session-start, on-session-end, handle-tool
}
```

### Plugin Discovery

1. `PluginManager::discover()` walks `~/.hermez/plugins/`
2. Each directory with `plugin.yaml` → `PluginManifest`
3. Manifest declares: tools, hooks, wasm/component entry points
4. `auto_load()` registers tools into `ToolRegistry` and hooks into `HookRegistry`

### Example Plugins

| Plugin | Type | Tools | Description |
|--------|------|-------|-------------|
| `example-wasm-plugin` | Phase-2 WASM | `greet` | Minimal C-ABI demo |
| `example-component-plugin` | Component Model | `greet` | wit-bindgen demo |
| `calc-plugin` | Component Model | `calc` | Math expression evaluator (+ − × ÷ ^, sqrt/sin/cos/ln...) |

### Build Toolchain

```bash
cd examples/calc-plugin
cargo component build --release  # produces plugin.component.wasm
```

## Key Statistics

| Metric | Count |
|--------|-------|
| Workspace crates | 12 |
| Default crates | 8 |
| Binary targets | 3 |
| Total modules | 70+ |
| Tool implementations | 55+ |
| Toolset definitions | 20+ |
| Platform adapters | 20 (20 implemented) |
| Provider types | 11 |
| LLM modules | 15 |
| Engine submodules | 14 |
| Prompt modules | 9 |
| CLI subcommands | 31 |
| ACP methods | 13 |
| Failover steps | 10 |
| Environment backends | 6 |
| Subagent delegation | depth=2, max 3 concurrent |
| Auxiliary fallback tiers | 5 |
| Reasoning extraction formats | 4 |
| Context compression stages | 4 |
| Web Dashboard pages | 6 |
| Web API endpoints | 12 |
| Plugin examples | 3 (2 demo + 1 utility) |
| CI/CD workflows | 1 (GitHub Actions) |
| Tool call provider parsers | 10 |
