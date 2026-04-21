# Hermes Agent Python → Rust 移植完成度评估

> 评估基准：原项目 `../hermes-agent`（Python） vs 当前 Rust workspace `hermez-ai`
> 评估时间：2026-04-21

---

## 1. 总体概况

| 维度 | Python 原项目 | Rust 当前项目 | 完成度 |
|------|--------------|---------------|--------|
| **业务代码** | 242,343 行 | 156,880 行 | ~65% 体量对齐 |
| **测试代码** | 214,475 行（611 文件） | ~22,500 行（2317 测试函数，33 集成测试文件） | ~10% ⚠️ |
| **Workspace crates** | — | 12 crates | — |
| **Binary targets** | 2 (cli + gateway) | 3 (hermes + hermes-agent + hermes-acp) | ✅ |
| **编译状态** | — | `cargo build --release` 通过 | ✅ |

> **加权综合完成度：约 65–70%**
> - 核心引擎/业务逻辑：**85–90%**
> - Skills 生态：**~100%**
> - 测试覆盖：**~2%**（最大短板）
> - Web UI：**~90%**（Dashboard + Sessions + Chat + Settings + Plugins + Export + Copy）
> - 外围生态（TUI/Website/Nix）：**~30%**（Nix + Benchmarks + Homebrew + CI）

---

## 2. 核心模块逐一对标

### 2.1 AI Agent 引擎

| Python 原文件 | Rust 对应 | 行数对比 | 状态 |
|--------------|-----------|---------|------|
| `run_agent.py` (12,113 行) | `hermes-agent-engine/src/` (12,700 行) | +587 | ✅ 已移植 |
| `agent/memory_manager.py` | `memory_manager.rs` | — | ✅ 已移植 |
| `agent/memory_provider.py` | `memory_provider.rs` | — | ✅ Trait 定义 |
| `agent/subagent.py` | `subagent.rs` | — | ✅ 已移植 |
| `agent/smart_model_routing.py` | `smart_model_routing.rs` | — | ✅ 已移植 |
| `agent/title_generator.py` | `title_generator.rs` | — | ✅ 已移植 |
| `agent/trajectory.py` | `trajectory.rs` | — | ✅ 已移植 |
| `agent/usage_pricing.py` | `usage_pricing.rs` | — | ✅ 已移植 |
| `agent/skill_commands.py` | `skill_commands.rs` | — | ✅ 已移植 |
| `agent/skill_utils.py` | `skill_utils.rs` | — | ✅ 已移植 |
| Self-evolution / review agent | `self_evolution.rs` / `review_agent.rs` | — | ✅ 已移植 |

**结论**：Agent 核心循环、对话状态机、子代理委派（depth≤2, max 3 concurrent）、记忆管理、轨迹保存、智能路由、自进化等全部对齐原项目。

---

### 2.2 LLM  provider 层

| Python 原文件 | Rust 对应 | 状态 |
|--------------|-----------|------|
| `agent/anthropic_adapter.py` | `hermes-llm/src/anthropic.rs` | ✅ |
| `agent/bedrock_adapter.py` | `hermes-llm/src/bedrock.rs` | ✅ |
| `agent/credential_pool.py` | `hermes-llm/src/credential_pool.rs` | ✅ |
| `agent/error_classifier.py` | `hermes-llm/src/error_classifier.rs` | ✅ |
| `agent/model_metadata.py` | `hermes-llm/src/model_metadata.rs` | ✅ |
| `agent/models_dev.py` | `hermes-llm/src/models_dev.rs` | ✅ |
| `agent/nous_rate_guard.py` | `hermes-llm/src/rate_limit.rs` | ✅ |
| `agent/retry_utils.py` | `hermes-llm/src/retry.rs` | ✅ |
| `agent/auxiliary_client.py` | `hermes-llm/src/auxiliary_client.rs` | ✅ |
| `_extract_reasoning` | `hermes-llm/src/reasoning.rs` | ✅ 4 formats |
| Tool call parsers | `hermes-llm/src/tool_call/*.rs` | ✅ 10 parsers |
| `agent/copilot_acp_client.py` | `hermes-llm/src/copilot_acp_client.rs` | ✅ |
| `agent/google_code_assist.py` | (部分并入 provider) | ⚠️ 需确认 |

**Provider 数量**：Python 原项目 11 个 → Rust 11 个（Anthropic, OpenAI, OpenRouter, Gemini, Codex, Kimi, Minimax, Z.ai, Nous, Bedrock, Custom）

---

### 2.3 Prompt / 上下文系统

| Python 原文件 | Rust 对应 | 状态 |
|--------------|-----------|------|
| `agent/prompt_builder.py` | `hermes-prompt/src/builder.rs` | ✅ + cache control |
| `agent/context_compressor.py` | `hermes-prompt/src/context_compressor.rs` | ✅ 4-stage |
| `agent/context_engine.py` | `hermes-prompt/src/context_engine.rs` | ✅ |
| `agent/context_references.py` | `hermes-prompt/src/context_references.rs` | ✅ |
| `agent/manual_compression_feedback.py` | `hermes-prompt/src/manual_compression_feedback.rs` | ✅ |
| `agent/subdirectory_hints.py` | `hermes-prompt/src/subdirectory_hints.rs` | ✅ |
| `agent/prompt_caching.py` | `hermes-prompt/src/cache_control.rs` | ✅ |
| `agent/redact.py` | `hermes-core/src/redact.rs` | ✅ |
| `trajectory_compressor.py` | `hermes-compress/src/` | ✅ |

---

### 2.4 Tools（~60 个工具模块）

| Python `tools/` (40,535 行) | Rust `hermes-tools/src/` (38,111 行) | 状态 |
|----------------------------|--------------------------------------|------|
| `file_tools.py` + `file_operations.py` | `file_ops.rs`, `shell_file_ops.rs` | ✅ |
| `terminal_tool.py` | `terminal.rs` | ✅ |
| `code_execution_tool.py` | `code_exec/` | ✅ |
| `browser_tool.py` + `browser_camofox*.py` | `browser/` (含 providers) | ✅ |
| `web_tools.py` | `web.rs` | ✅ |
| `delegate_tool.py` | `delegate.rs` | ✅ |
| `skills_tool.py` + `skills_guard.py` + `skills_sync.py` + `skills_hub.py` | `skills/`, `skills_guard.rs`, `skills_sync.rs`, `skills_hub.rs` | ✅ |
| `memory_tool.py` | `memory.rs` | ✅ |
| `mcp_tool.py` + `mcp_oauth*.py` | `mcp_client/`, `mcp_oauth.rs` | ✅ |
| `mcp_serve.py` | `mcp_serve.rs` | ✅ |
| `send_message_tool.py` | `send_message.rs` | ✅ |
| `cronjob_tools.py` | `cron_tools.rs` | ✅ |
| `approval.py` | `approval.rs` | ✅ |
| `todo_tool.py` | `todo.rs` | ✅ |
| `checkpoint_manager.py` | `checkpoint.rs` | ✅ |
| `image_generation_tool.py` | `image_gen.rs` | ✅ |
| `vision_tools.py` | `vision.rs` | ✅ |
| `voice_mode.py` + `tts_tool.py` + `transcription_tools.py` | `voice.rs`, `tts.rs`, `transcription.rs` | ✅ |
| `feishu_doc_tool.py` + `feishu_drive_tool.py` | `feishu.rs` | ✅ |
| `homeassistant_tool.py` | `homeassistant.rs` | ✅ |
| `rl_training_tool.py` | `rl_training.rs` | ✅ |
| `managed_tool_gateway.py` | `managed_tool_gateway.rs` | ✅ |
| `mixture_of_agents_tool.py` | `moa.rs` | ✅ |
| `env_passthrough.py` | `env_passthrough.rs` | ✅ |
| `session_search_tool.py` | `session_search.rs` | ✅ |
| `url_safety.py` | `url_safety.rs` | ✅ |
| `osv_check.py` | `osv_check.rs` | ✅ |
| `neutts_synth.py` | `neutts_synth.rs` | ✅ |
| `openrouter_client.py` | `openrouter_client.rs` | ✅ |
| `patch_parser.py` | `patch_parser.rs` | ✅ |
| `fuzzy_match.py` | `fuzzy_match.rs` | ✅ |
| `toolsets.py` + `toolset_distributions.py` | `toolsets_def.rs` | ✅ 20+ toolsets |
| `registry.py` | `registry.rs` | ✅ |

**环境后端**：local / docker / ssh / daytona / singularity / modal —— 全部 6 种已对齐。

---

### 2.5 Gateway（19+ 平台适配器）

| Python `gateway/` (49,028 行) | Rust `hermes-gateway/src/` (39,217 行) | 状态 |
|------------------------------|----------------------------------------|------|
| `gateway/run.py` | `runner.rs` | ✅ |
| `gateway/session.py` | `session.rs` | ✅ |
| `gateway/delivery.py` | `delivery.rs` | ✅ |
| `gateway/stream_consumer.py` | `stream_consumer.rs` | ✅ |
| `gateway/platforms/telegram*.py` | `platforms/telegram.rs` + `telegram_network.rs` | ✅ |
| `gateway/platforms/discord.py` | `platforms/discord.rs` | ✅ |
| `gateway/platforms/slack.py` | `platforms/slack.rs` | ✅ |
| `gateway/platforms/feishu*.py` | `platforms/feishu.rs` + `feishu_ws.rs` + `feishu_comment.rs` + `feishu_comment_rules.rs` | ✅ |
| `gateway/platforms/weixin.py` | `platforms/weixin.rs` | ✅ |
| `gateway/platforms/wecom*.py` | `platforms/wecom.rs` + `wecom_callback.rs` | ✅ |
| `gateway/platforms/dingtalk.py` | `platforms/dingtalk.rs` | ✅ |
| `gateway/platforms/whatsapp.py` | `platforms/whatsapp.rs` | ✅ |
| `gateway/platforms/email.py` | `platforms/email.rs` | ✅ |
| `gateway/platforms/matrix.py` | `platforms/matrix.rs` | ✅ |
| `gateway/platforms/signal.py` | `platforms/signal.rs` | ✅ |
| `gateway/platforms/homeassistant.py` | `platforms/homeassistant.rs` | ✅ |
| `gateway/platforms/bluebubbles.py` | `platforms/bluebubbles.rs` | ✅ |
| `gateway/platforms/mattermost.py` | `platforms/mattermost.rs` | ✅ |
| `gateway/platforms/qqbot.py` | `platforms/qqbot.rs` | ✅ |
| `gateway/platforms/webhook.py` | `platforms/webhook.rs` | ✅ |
| `gateway/platforms/api_server.py` | `platforms/api_server.rs` | ✅ |

**20 个平台适配器全部到位**（Python 原项目也是 20 个）。

---

### 2.6 CLI（31+ 子命令）

| Python `cli.py` + `hermes_cli/` (~61K 行) | Rust `hermes-cli/src/` (21,599 行) | 状态 |
|------------------------------------------|-------------------------------------|------|
| 31+ subcommands | 31+ subcommands | ✅ 对齐 |
| TUI / reedline | `tui/` (completers, curses, input, voice) | ✅ |
| Setup wizard | `setup_cmd.rs` | ✅ |
| OAuth flows | `oauth_flow.rs` / `oauth_server.rs` | ✅ |
| Backup | `backup_cmd.rs` | ✅ |
| Gateway mgmt | `gateway_mgmt.rs` | ✅ |
| Cron mgmt | `cron_cmd.rs` | ✅ |
| Batch runner | `batch_cmd.rs` | ✅ |
| Skills hub | `skills_hub_cmd.rs` | ✅ |
| Web server | `web_server.rs` | ✅ |
| Skin engine | `skin_engine.rs` | ✅ |

> 注：Rust CLI 代码更紧凑（21K vs 61K），部分原因是原项目 hermes_cli/ 包含了很多 config/model/provider 相关的逻辑，这些在 Rust 中被下沉到了 core/llm 层。

---

### 2.7 状态 / 数据库

| Python | Rust | 状态 |
|--------|------|------|
| `hermes_state.py` (1,293 行) | `hermes-state/src/` (2,490 行) | ✅ 对齐 + FTS5 |
| SQLite schema | `schema.rs` + `models.rs` | ✅ |
| Insights engine | `insights.rs` | ✅ |

---

### 2.8 其他已对齐模块

| Python | Rust | 状态 |
|--------|------|------|
| `batch_runner.py` | `hermes-batch/src/` | ✅ |
| `trajectory_compressor.py` | `hermes-compress/src/` | ✅ |
| `cron/jobs.py` + `scheduler.py` | `hermes-cron/src/` | ✅ |
| `mcp_serve.py` | `hermes-tools/src/mcp_serve.rs` | ✅ |
| `acp_adapter/` | `src/hermes_acp/` | ✅ 13 methods |
| `rl_cli.py` + `mini_swe_runner.py` + `tinker-atropos` | `hermes-rl/src/` | ✅ 5 envs |
| `hermes_constants.py` | `hermes-core/src/constants.rs` | ✅ |
| `hermes_logging.py` | `hermes-core/src/logging.rs` | ✅ |
| `config.py` | `hermes-core/src/config.rs` | ✅ + env expand |

---

## 3. Skills 生态

| 来源 | SKILL.md 数量 |
|------|--------------|
| Python `skills/` | 77 |
| Python `optional-skills/` | 50 |
| **Python 合计** | **127** |
| **Rust `skills/`** | **127** |

✅ **Skills 1:1 完整移植**。原项目的 `optional-skills/` 在 Rust 版本中被合并进了 `skills/` 目录（增加了 `blockchain`, `communication`, `health`, `migration`, `security` 等顶层分类）。

---

## 4. 明确缺失 / 未移植

### 4.1 完全未开始

| 模块 | 原项目规模 | 影响评估 |
|------|-----------|---------|
| **ui-tui** (React Ink CLI UI) | 229 文件 | 🔴 高 — 原项目有完整的 TUI 界面（hermes-ink），Rust 项目无对应 |
| **website** (Docusaurus 文档站) | 146 文件 | 🟡 中 — 面向用户的文档站，不影响运行时 |
| **tinker-atropos** (RL 子模块) | 18 文件 | 🟡 中 — 独立 RL 训练环境，RL crate 已覆盖核心 |
| **nix** (Nix flake 支持) | 8 文件 | `flake.nix` 已提供开发 Shell + 包构建 | 🟢 低 — 开发环境/部署 |
| **packaging** (Homebrew 公式) | 2 文件 | `packaging/hermes.rb` 模板已提供 | 🟢 低 — 发布打包 |
| **environments/benchmarks** | 43 文件 | `benchmarks/` 骨架 + `run.sh` 已提供 | 🟡 中 — TerminalBench2 等评测环境 |

### 4.2 大幅缩水

| 模块 | 原项目 | Rust 项目 | 评估 |
|------|--------|-----------|------|
| **Web 前端** | web/src: 7,301 行 (React/Vite) | web/src: ~2,700 行 (Dashboard/Sessions/Detail/Status/Settings/Plugins + Chat + Markdown + Export + Copy) | 🟢 高 — 核心功能+高级功能到位，消息导出、代码块复制、Markdown 渲染完整 |
| **Plugins** | 13 目录 (含 context_engine, memory plugins) | 4 个 plugin (calc + time + string + example-wasm) + Web UI 管理 | 🟢 高 — Component Model 链路打通，实用插件生态起步 |
| **Tests** | 611 文件，214K 行 | ~22,500 行（2,284 单元 + 33 集成测试） | 🟡 核心 crate 已覆盖，总量仍有差距 |

---

## 5. 质量与成熟度差距

### 5.1 测试覆盖（最大风险）

- **原项目**：611 个测试文件，覆盖 agent 循环、gateway 平台适配、CLI 命令、tools 边界、压缩策略、安全沙箱等。
- **Rust 项目**：12 个 crate 共 2,284 个单元测试 + 33 个集成测试，约 22,500 行测试代码。核心 crate（core/state/llm/tools/prompt）已基本覆盖，agent-engine/gateway/cli 次之。
- **风险**：hermes-agent-engine 中 1 个 subagent 测试（`test_delegation_filters_blocked_toolsets`）已标记 `#[ignore]`，需 mock LLM 调用链才能快速回归；gateway 平台适配、CLI 端到端、安全沙箱的覆盖仍有缺口。

### 5.2 文档站

- 原项目有完整的 Docusaurus 网站（146 文件），含开发者指南、用户指南、API 参考。
- Rust 项目仅有 `docs/ARCHITECTURE.md`, `docs/USAGE.md`, `docs/E2E_TEST.md` 三份内部文档。

### 5.3 E2E 测试

- 原项目有 `tests/e2e/` 和大量 gateway/cli 的端到端测试。
- Rust 项目有 `scripts/e2e_test.sh`，但仅覆盖 ~60 个 CLI 命令的基本调用，无 API 交互验证。

---

## 6. 分层完成度一览

```
Tier 5 — Binary Targets          ████████████████████ 100% ✅
  hermes / hermes-agent / hermes-acp 全部产出

Tier 4 — CLI / Adapter Layer     ██████████████████░░  90% ✅
  31 子命令 ✅ | Gateway 20 平台 ✅ | TUI 基本 ✅ | Web Dashboard 核心功能 ✅
  缺失：React Ink TUI、Web 前端高级功能（图表、主题）

Tier 3 — Agent Engine Layer      ███████████████████░  95% ✅
  AIAgent 核心循环 ✅ | 子代理 ✅ | 自进化 ✅ | 预算 ✅ | 压缩 ✅
  缺失：部分高级 gateway hooks、镜像模式

Tier 2 — Service Layer           ████████████████████ 100% ✅
  55+ tools ✅ | 20+ toolsets ✅ | Prompt builder ✅ | Registry ✅

Tier 1 — Infrastructure Layer    ███████████████████░  95% ✅
  11 providers ✅ | SQLite + FTS5 ✅ | Credential pool ✅ | Retry ✅
  缺失：Google OAuth 独立适配器

Tier 0 — Core Layer              ████████████████████ 100% ✅
  Config ✅ | Error ✅ | Logging ✅ | Home path ✅ | Redaction ✅

外围生态                          ██████░░░░░░░░░░░░░░  30% 🟡
  Nix ✅ | Benchmarks 骨架 ✅ | Homebrew 模板 ✅ | CI ✅ | Plugins 管理 UI ✅
  Website ❌ | ui-tui ❌ | Plugin 实用数量仍有增长空间

测试覆盖                          █████░░░░░░░░░░░░░░░  ~10% 🟡
  单元测试：core 79 | state 45 | llm 446 | tools 711 | prompt 80 | agent 274 | gateway 333 | cli 174 | cron 34 | batch 42 | compress 47 | rl 52
  集成测试：每 crate 2–4 个，共 33 个
  CI/CD：GitHub Actions workflow 已配置 ✅
  E2E 仅 CLI 命令级 ⚠️
```

---

## 7. 结论与建议

### 7.1 已完成的部分（可直接投产）

- **核心对话引擎**：Agent 循环、工具调用、failover 链、上下文压缩、子代理 —— 功能完整。
- **Gateway 多平台**：20 个消息平台适配器全部就绪，配置接口对齐。
- **Skills 生态**：127 个 skill 1:1 移植，用户侧无感知差异。
- **CLI 命令集**：31+ 子命令完整，含 setup、backup、gateway、cron、batch 等。

### 7.2 必须补强的短板

1. **测试体系（P0）**
   - 当前 11 个集成测试 vs 原项目 611 个测试文件。
   - 建议按 crate 逐步迁移高价值测试：先 `hermes-llm`（provider mock），再 `hermes-tools`（边界/安全），再 `hermes-gateway`（平台适配 mock）。

2. **Web 前端（P1）**
   - `web/` 目前只有 417 行骨架，原项目有 7,301 行功能完整的 React 前端。
   - 需要重新实现或至少恢复核心页面（chat、session browser、settings）。

3. **Plugin 架构（P1）**
   - 原项目有 13 个 plugin 目录（含 memory 后端、context engine）。
   - Rust 项目已有 3 个实用插件（calc、time、string）+ 1 个 example，Component Model 开发链路完整。

4. **可选技能与评测环境（P2）**
   - `optional-skills/` 已合并进 `skills/`，但 `environments/benchmarks/` 和 `tinker-atropos` 未移植。
   - 不影响日常运行，但阻碍 RL/SWE 能力验证。

5. **外围生态（P2）**
   - `nix/` ✅、`benchmarks/` ✅、`packaging/` ✅、`.github/workflows/` ✅ 已补充。
   - `website/`、`ui-tui/` 可后续补充。

---

*报告生成完毕。如需深入某个 crate 的详细 diff 或测试缺口分析，可继续指定。*
