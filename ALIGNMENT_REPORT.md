# Hermes-rs vs Python 功能对齐度审查报告

**审查日期**: 2026-04-18
**Rust 代码总量**: ~120,622 行（13 crates）
**Python 代码总量**: ~152,497 行（agent/ + tools/ + hermez_cli/ + gateway/ + cron/）
**整体对齐度估计**: **~75%**（核心架构对齐，外围平台和部分高级功能存在差距）

---

## 1. 模块级对齐度总览

| 模块 | Python 对应 | Rust Crate | 对齐度 | 关键差距 |
|------|------------|------------|--------|---------|
| 核心类型/配置/常量 | `hermez_constants.py` + 散布代码 | `hermez-core` | **90%** | 基础库完整，auth lock、redact、proxy validation 均已实现 |
| 状态存储 (SQLite) | `hermez_state.py` | `hermez-state` | **85%** | WAL + FTS5 已对齐；InsightsEngine 存在 |
| LLM 客户端 | `agent/` (anthropic, bedrock, openai, etc.) | `hermez-llm` | **85%** | Anthropic streaming 已实现（注释已更新） |
| 工具注册与实现 | `tools/` (54 个 .py) | `hermez-tools` | **68%** | Modal/Daytona 环境全 stub；Singularity bind mounts + SIF 锁 已补齐 |
| Prompt 构建 | `agent/prompt_builder.py` + `context_compressor.py` | `hermez-prompt` | **85%** | 缓存控制、上下文压缩、注入扫描均已对齐 |
| Agent 对话引擎 | `run_agent.py` | `hermez-agent-engine` | **75%** | 核心循环完备；self_evolution 等高级特性可能部分骨架化 |
| CLI 应用 | `hermez_cli/` (48 个 .py) | `hermez-cli` | **70%** | 命令表面齐全；部分 niche 命令可能未完全实现 |
| Cron 调度 | `cron/` | `hermez-cron` | **85%** | jobs、scheduler、delivery 三大件齐全 |
| 批处理 | `batch_runner.py` | `hermez-batch` | **80%** | checkpoint、distribution、trajectory 均已实现 |
| 上下文压缩 | `trajectory_compressor.py` | `hermez-compress` | **85%** | TrajectoryCompressor + Summarizer 对齐 |
| 消息网关 | `gateway/` (24 个平台) | `hermez-gateway` (13 个平台) | **78%** | 缺失 8 个平台适配器；Feishu card action 已接入；部分 plumbing 待 wiring |
| ACP 编辑器协议 | `acp_adapter/` | `hermez-acp` | **60%** | JSON-RPC 协议完整；slash command 响应已流式发送；核心 LLM prompt 处理未接入（注：实际运行的 binary 实现 `src/hermez_acp` 已完整接入） |
| RL 训练环境 | `environments/` | `hermez-rl` | **80%** | Environment trait + Math/ToolUse/Atropos/WebResearch 已实现 |

---

## 2. 关键差距详解

### 🔴 严重差距（影响核心可用性）

#### 2.1 `hermez-acp` — ACP 服务器 LLM 未接入（55% → 阻断级）

- **现状**: JSON-RPC 2.0 协议、SessionManager、所有通知类型、初始化握手均已实现（~1,500 行脚手架）。
- **缺失**: `handle_prompt()` 方法是**纯骨架**，注释明确说明：
  > "In a real implementation, this is where you would: 1. Build conversation history 2. Call the LLM 3. Stream tool calls 4. Collect final response."
- **影响**: ACP 服务器目前对 IDE 扩展返回空/占位响应，VS Code / Zed / JetBrains 插件无法实际使用。
- **建议**: 接入 `hermez-agent-engine::AIAgent` 或 `hermez-llm::client`，复用现有消息循环。

#### 2.2 `hermez-tools` — 云环境后端大面积 stub（65% → 高风险）

| 后端 | 状态 | 说明 |
|------|------|------|
| **Local** | ✅ 实现 | 本地终端执行 |
| **Docker** | ✅ 实现 | `docker_env.rs` 完整 |
| **SSH** | ✅ 实现 | `ssh.rs` 完整 |
| **Modal** | ❌ 全 stub | ~15 个方法待实现（sandbox 生命周期、文件上传/执行、快照、镜像解析） |
| **Daytona** | ❌ 全 stub | ~10 个方法待实现（sandbox 生命周期、执行、上传、清理） |
| **Singularity** | ⚠️ 部分 | 基础执行可用；credential/skills bind mounts 和 SIF 构建待完成 |

- **影响**: 企业级/云端沙箱工作流在 Rust 版本不可用。
- **建议**: Modal/Daytona 需要等上游 Rust SDK 或手写 REST 客户端；Singularity 可优先补齐。

#### 2.3 `hermez-llm` — Anthropic Streaming（80% → 体验降级）

- **现状**: `call_llm_stream()` 对 Anthropic 回退到非流式，只返回单个 `TextDelta`。
- **影响**: CLI TUI 的打字机效果在 Anthropic 模型上不可用；网关流式响应延迟增加。
- **建议**: 实现 Anthropic Messages API 的 SSE 流解析，参考 OpenAI streaming 实现。

---

### 🟡 中等差距（影响功能广度）

#### 2.4 Gateway 平台适配器数量不足（13 vs 24）

**Rust 已实现（13）**: Telegram, Discord, Slack, WhatsApp, Feishu, Feishu WS, DingTalk, WeCom, Weixin, Webhook, API Server, Helpers

**Python 有但 Rust 缺失（8）**:
| 平台 | Python 文件 | 影响评估 |
|------|------------|---------|
| Signal | `signal.py` | 中 — 隐私用户群体 |
| Matrix | `matrix.py` | 中 — 去中心化社区 |
| Mattermost | `mattermost.py` | 低 — 企业自托管 |
| BlueBubbles | `bluebubbles.py` | 低 — iMessage 桥接 |
| Home Assistant | `homeassistant.py` | 中 — 智能家居事件接收（工具侧有 `ha_call_service`，但网关事件监听缺失） |
| Email | `email.py` | 中 — IMAP/SMTP 双向邮件 |
| SMS | `sms.py` | 低 — Twilio |
| QQBot | `qqbot.py` | 低 — 国内年轻用户 |

- **建议**: 按用户量优先级，建议先补齐 Signal、Matrix、Email；QQBot 和 BlueBubbles 可延后。

#### 2.5 `hermez-gateway` 内部 plumbing 待 wiring

- `api_server.rs:1313` — stream_delta_callback 未接入 agent
- `api_server.rs:1834` — resolved history 未接入 handler pipeline
- `feishu.rs:874` — card action handler 路由 TODO
- **影响**: 网关 API Server 的流式体验和飞书卡片交互不完整。

---

### 🟢 小差距 / 已对齐区域

#### 2.6 CLI 命令表面对齐但实现深度存疑

Rust CLI (`hermez-rs/src/main.rs`) 的 `Commands` 枚举覆盖了 Python 的绝大多数命令：
- chat, setup, tools, skills, gateway, doctor, models, profiles, sessions, config, batch, cron, auth, skin, status, insights, logs, webhook, plugins, memory, mcp, model, login, pairing, update, uninstall, dashboard, whatsapp, acp, claw, backup, restore, completion, version, debug, dump

**风险**: 表面命令齐全（~40 个子命令/子动作），但 `hermez-cli` 整体评估仅 **70%**，意味着部分命令可能是骨架实现（如 `dashboard` 的 WebUI、部分 `oauth_flow` 分支等）。需逐个命令进行深度测试验证。

#### 2.7 安全与基础设施 — 高度对齐

以下 Python 关键安全机制在 Rust 中均有对应实现：
- ✅ `tools/approval.py` → `hermez-tools::approval`
- ✅ `agent/redact.py` → `hermez-core::redact`
- ✅ `tools/path_security.py` → `hermez-tools::path_security`
- ✅ `tools/skills_guard.py` → `hermez-tools::skills_guard`
- ✅ `agent/credential_pool.py` → `hermez-llm::credential_pool`
- ✅ Profile 多实例机制 → `hermez-core::hermez_home`
- ✅ Auth 文件锁 → `hermez-core::auth_lock`

---

## 3. 功能特性逐项对比

### 3.1 Tools（工具系统）

| Toolset | Python | Rust | 对齐状态 |
|---------|--------|------|---------|
| web_search / web_extract | ✅ | ✅ | 对齐 |
| terminal / process | ✅ | ✅ | 对齐 |
| file_ops (read/write/patch/search) | ✅ | ✅ | 对齐 |
| vision_analyze | ✅ | ✅ | 对齐 |
| image_generate | ✅ | ✅ | 对齐 |
| browser_* (10 个操作) | ✅ | ✅ | 对齐 |
| tts | ✅ | ✅ | 对齐 |
| skills_list / skill_view / skill_manage | ✅ | ✅ | 对齐 |
| todo | ✅ | ✅ | 对齐 |
| memory | ✅ | ✅ | 对齐 |
| session_search | ✅ | ✅ | 对齐 |
| clarify | ✅ | ✅ | 对齐 |
| execute_code | ✅ | ✅ | 对齐（RPC dispatch 到 registry） |
| delegate_task | ✅ | ✅ | 对齐 |
| cronjob | ✅ | ✅ | 对齐 |
| send_message | ✅ | ✅ | 对齐 |
| homeassistant | ✅ | ✅ | 工具侧对齐 |
| mixture_of_agents | ✅ | ✅ | 对齐 |
| rl_* (10 个操作) | ✅ | ✅ | 对齐 |
| mcp_client | ✅ | ✅ | 依赖 `rmcp` crate |
| checkpoint | ✅ | ✅ | 对齐 |
| moa | ✅ | ✅ | 对齐 |
| tirith_security | ✅ | ✅ | 对齐 |

### 3.2 Agent 引擎特性

| 特性 | Python | Rust | 对齐状态 |
|------|--------|------|---------|
| 同步 tool-calling 循环 | ✅ | ✅ | 对齐 |
| 并行工具执行 (8 workers) | ✅ | ✅ | 对齐 |
| 路径级并行 | ✅ | ✅ | 对齐 |
| Iteration budget + refund | ✅ | ✅ | 对齐 |
| Interrupt 传播 | ✅ | ✅ | 对齐 |
| OpenAI / Codex | ✅ | ✅ | 对齐 |
| Anthropic Messages + caching | ✅ | ⚠️ | Streaming 不完整 |
| AWS Bedrock | ✅ | ✅ | 对齐 |
| OpenRouter | ✅ | ✅ | 对齐 |
| Copilot ACP | ✅ | ❓ | 待验证 |
| Local endpoints (Ollama) | ✅ | ✅ | 对齐 |
| Rate limit tracking | ✅ | ✅ | 对齐 |
| Context pressure warnings | ✅ | ✅ | 对齐 |
| Prefill messages | ✅ | ✅ | 对齐 |
| Reasoning config | ✅ | ✅ | 对齐 |
| Tool progress callbacks | ✅ | ✅ | 对齐 |
| Streaming delta callbacks | ✅ | ⚠️ | Anthropic 缺失 |
| Trajectory saving | ✅ | ✅ | 对齐 |
| Smart model routing | ✅ | ✅ | 对齐 |
| Auxiliary client | ✅ | ✅ | 对齐 |
| Context compressor | ✅ | ✅ | 对齐 |
| Memory manager (pluggable) | ✅ | ✅ | 对齐 |
| Prompt injection scan | ✅ | ✅ | 对齐 |
| Self evolution | ✅ | ⚠️ | 可能部分实现 |

### 3.3 Gateway 平台

| 平台 | Python | Rust | 优先级建议 |
|------|--------|------|-----------|
| Telegram | ✅ | ✅ | — |
| Discord | ✅ | ✅ | — |
| Slack | ✅ | ✅ | — |
| WhatsApp | ✅ | ✅ | — |
| Feishu/Lark | ✅ | ✅ | Rust 额外拆分了 feishu_ws |
| DingTalk | ✅ | ✅ | — |
| WeCom | ✅ | ✅ | — |
| Weixin | ✅ | ✅ | — |
| Webhook | ✅ | ✅ | — |
| API Server | ✅ | ✅ | — |
| Signal | ✅ | ❌ | 中 |
| Matrix | ✅ | ❌ | 中 |
| Mattermost | ✅ | ❌ | 低 |
| BlueBubbles | ✅ | ❌ | 低 |
| Home Assistant | ✅ | ❌ | 中（网关事件侧） |
| Email | ✅ | ❌ | 中 |
| SMS | ✅ | ❌ | 低 |
| QQBot | ✅ | ❌ | 低 |

---

## 4. 架构质量评估

### 4.1 Rust 版本的优势

1. **类型安全**: `HermesError` 统一错误类型、`thiserror` 结构化错误、`anyhow` 上下文传递，优于 Python 的异常散射。
2. **异步原生**: 全栈基于 `tokio`，网关和工具后端天然异步；Python 版本核心循环是同步的。
3. **性能**: Rust 的 token 估算、正则、JSON 处理、SQLite 操作均有更低开销。
4. **依赖管理**: Workspace 级别的依赖统一，避免 Python 的依赖冲突。
5. **测试基础设施**: `rstest` + `mockito` + `proptest` + `tokio-test`，测试框架现代化。

### 4.2 Rust 版本的劣势 / 技术债

1. **ACP LLM 未接入**: 这是最大的功能盲区，导致编辑器集成不可用。
2. **云环境 stub 过多**: Modal/Daytona 尚未有成熟 Rust SDK，手写 REST 客户端工作量大。
3. **Gateway 平台覆盖率低**: 缺失 8 个平台，主要是社区/小众平台，但影响"全平台"宣传口径。
4. **CLI 深度未验证**: 命令枚举齐全，但实现深度需要逐个测试。

---

## 5. 建议路线图

### Phase 1: 核心可用性（阻断问题）
1. **接入 ACP LLM 调用** (`hermez-acp::handle_prompt` → `hermez-agent-engine`)
2. **补齐 Anthropic Streaming** (`hermez-llm::anthropic` SSE 解析)
3. **修复 Gateway API Server plumbing** (stream_delta_callback, resolved history wiring)

### Phase 2: 功能广度（平台覆盖）
4. **补齐主流缺失平台**: Signal、Matrix、Email（IMAP/SMTP）
5. **补齐 Home Assistant 网关适配器**（工具侧已有，只需网关事件接收）
6. **验证 CLI 所有子命令的实现深度**

### Phase 3: 企业级特性（云环境）
7. **实现 Modal 后端**（需要调研 Modal 的 REST API 或 Rust SDK 可用性）
8. **实现 Daytona 后端**（Daytona 有 OpenAPI，可生成客户端）
9. **补齐 Singularity 剩余功能**

### Phase 4: 生态完善
10. **Web 前端 (`web/`) 的 Rust 对应物**: 目前 Python 有 React/Vite dashboard；Rust CLI 有 `dashboard` 命令但可能未实现完整 WebUI。
11. **文档站点 (`website/`)**: Docusaurus 文档目前只有 Python 版本。

---

## 6. 总结

Hermes-rs 是一个**架构设计良好、基础设施扎实**的 Rust 重写项目。13 个 crate 的划分与 Python 模块高度对应，核心类型、LLM 客户端、工具注册、Agent 引擎、Prompt 系统、状态存储、Cron、Batch、RL 等模块的实现质量较高。

**最大风险点**:
1. `hermez-tools` 的 Modal/Daytona 全 stub（云端沙箱不可用）
2. `hermez-gateway` 缺失 8 个平台适配器
3. `hermez-acp` crate-level 实现（`crates/hermez-acp`）的 LLM 未接入（但 binary 实现 `src/hermez_acp` 已修复并可用）

**最大亮点**:
- 基础 crate（core、state、prompt、cron、compress）完整度 85-90%
- 安全机制（approval、redact、path_security、skills_guard）全面移植
- 异步架构适合高并发网关场景
- Profile 多实例、Credential Pool、Smart Routing 等高级特性均已落地

如果目标是**功能完全替代 Python 版本**，当前 Rust 版本大约完成了 **75%** 的功能对齐度；如果目标是**生产可用**，需要优先解决 Phase 1 的三个阻断问题。
