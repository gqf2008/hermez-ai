# Python vs Rust 架构对比

## 1. 整体结构对比

| 维度 | Python 版 | Rust 版 | 状态 |
|------|-----------|---------|------|
| **代码组织** | 扁平: 顶层 9 个 .py + agent/ tools/ gateway/ hermez_cli/ | 12 个 crate, 5 层 DAG, 严格分层 | Rust 更清晰 |
| **最大文件** | `gateway/run.py` 460KB, `run_agent.py` 11487 行 | 最大 ~350 行 (agent.rs) | Rust 拆分更细 |
| **入口点** | 3 (hermes, hermez-agent, hermez-acp) | 3 (同左) | 对齐 |
| **配置管理** | `hermez_cli/config.py` 143KB | `hermez-core/config.rs` ~400 行 | Rust 更精简 |
| **测试覆盖** | 577 测试文件 | 单元测试 + 48 个 E2E bash 用例 | Rust 待加强 |

## 2. 模块级对比

### Agent Engine

| 功能 | Python 文件 | Rust 模块 | 差异 |
|------|-------------|-----------|------|
| 核心对话循环 | `run_agent.py` (11487 行) | `agent.rs` (~350 行) | Python 单文件巨型类; Rust 拆到 14 个子模块 |
| 凭证池 | `agent/credential_pool.py` (60KB) | `hermez-llm/credential_pool.rs` | 对齐 |
| 辅助模型 | `agent/auxiliary_client.py` (116KB) | `hermez-llm/auxiliary_client.rs` | 对齐, 5 级降级 |
| Anthropic 适配 | `agent/anthropic_adapter.py` (60KB) | `hermez-llm/anthropic.rs` | 对齐 |
| Bedrock 适配 | `agent/bedrock_adapter.py` (43KB) | ❌ 未移植 | 缺失 |
| Prompt 构建 | `agent/prompt_builder.py` (47KB) | `hermez-prompt/builder.rs` | 对齐 |
| 上下文压缩 | `agent/context_compressor.py` (50KB) | `hermez-prompt/context_compressor.rs` | 4 阶段对齐 |
| 错误分类 | `agent/error_classifier.py` (29KB) | `hermez-llm/error_classifier.rs` | Failover 链 10 步对齐 |
| 定价 | `agent/usage_pricing.py` (25KB) | `hermez-llm/pricing.rs` | 对齐 |
| 模型元数据 | `agent/model_metadata.py` (45KB) | `hermez-llm/models_dev.rs` | 对齐 |
| 模型路由 | `agent/smart_model_routing.py` (5KB) | `hermez-agent-engine/smart_model_routing.rs` | 对齐 |
| 子代理 | (内嵌在 run_agent.py) | `hermez-agent-engine/subagent.rs` | Rust 更独立 |
| 标题生成 | `agent/title_generator.py` (4KB) | `hermez-agent-engine/title_generator.rs` | 对齐 |
| 记忆管理 | `agent/memory_manager.py` (14KB) | `hermez-agent-engine/memory_manager.rs` | 对齐 |
| Memory Provider | `agent/memory_provider.py` (10KB) | `hermez-agent-engine/memory_provider.rs` | Trait 定义对齐 |
| 技能命令 | `agent/skill_commands.py` (14KB) | `hermez-agent-engine/skill_commands.rs` | 对齐 |
| 注入防御 | — | `hermez-prompt/injection_scan.rs` | **Rust 新增** |
| Prompt 缓存 | `agent/prompt_caching.py` (2KB) | `hermez-prompt/cache_control.rs` | 对齐 |
| Self Evolution | — | `hermez-agent-engine/self_evolution.rs` | **Rust 新增** |
| Review Agent | — | `hermez-agent-engine/review_agent.rs` | **Rust 新增** |
| Trajectory | `agent/trajectory.py` (2KB) | `hermez-agent-engine/trajectory.rs` | 对齐 |
| 速率限制 | `agent/rate_limit_tracker.py` (8KB) | `hermez-llm/rate_limit.rs` | 对齐 |
| 上下文引擎 | `agent/context_engine.py` (6KB) | ❌ 未移植 | 缺失 (可合并到 prompt builder) |
| Nous Rate Guard | `agent/nous_rate_guard.py` (5KB) | ❌ 未移植 | 缺失 (可合并到 rate_limit) |
| 手动压缩反馈 | `agent/manual_compression_feedback.py` (1KB) | `hermez-prompt/manual_compression_feedback.rs` | 对齐 |
| 子目录提示 | `agent/subdirectory_hints.py` (8KB) | `hermez-prompt/subdirectory_hints.rs` | 对齐 |
| 上下文引用 | `agent/context_references.py` (17KB) | `hermez-prompt/context_references.rs` | 对齐 |
| Soul/身份 | — | `hermez-prompt/soul.rs` | **Rust 对齐 Python 的 soul.md 逻辑** |
| 推理提取 | (内嵌在 run_agent.py) | `hermez-llm/reasoning.rs` | Rust 独立, 4 种格式 |
| Tool Call 解析 | (内嵌在 run_agent.py) | `hermez-llm/tool_call/` (10 provider 解析器) | **Rust 更完善** |
| Retry 工具 | `agent/retry_utils.py` (1KB) | `hermez-llm/retry.rs` | 对齐 |
| Display/显示 | `agent/display.py` (41KB) | ❌ 未移植 | TUI 层, Rust 在 hermez-cli 中 |
| Insights | `agent/insights.py` (34KB) | `hermez-state/insights.rs` | 对齐 |
| Redact/PII | `agent/redact.py` (8KB) | (内嵌在 gateway/session.rs) | 对齐 |
| Skill Utils | `agent/skill_utils.py` (16KB) | ❌ 未移植 | Rust 在 skill_commands 中 |
| Copilot ACP | `agent/copilot_acp_client.py` (20KB) | ❌ 未移植 | VS Code 专用 |

### Tools

| 维度 | Python | Rust | 差异 |
|------|--------|------|------|
| 工具文件数 | 54 顶层 + 5 browser_providers = 59 | 55+ 文件 | 对齐 |
| 注册方式 | 模块级 `registry.register()` (import-time 自动注册) | `register_all_tools()` (启动时集中注册) | **Rust 需手动维护** |
| 环境后端 | 8 (local/docker/modal/managed_modal/singularity/ssh/daytona) | 6 (local/docker/singularity/ssh/daytona/modal) | Rust 缺 managed_modal |
| Toolset 定义 | `toolsets.py` (702 行, 20+ toolsets) | `toolsets_def.rs` (20+ toolsets) | 对齐 |
| MCP 集成 | `mcp_tool.py` | `mcp_client/` 目录 | Rust 更模块化 |

### Gateway

| 维度 | Python | Rust | 差异 |
|------|--------|------|------|
| 核心文件 | `run.py` (460KB) + `base.py` (169KB) + 5 个辅助 | `runner.rs` + `session.rs` + 4 个辅助 | Rust 更精简 |
| 平台适配器 | **18 个** | **5 个** (api_server, dingtalk, feishu, wecom, weixin) | **Rust 缺失 13 个** |
| 缺失平台 | — | telegram, discord, slack, matrix, signal, whatsapp, qqbot, email, sms, homeassistant, mattermost, bluebubbles, webhook | 待移植 |
| 平台枚举 | 17 种 | 19 种 (Rust 多了 Local, ApiServer) | 枚举已定义 |
| 会话管理 | `session.py` (42KB) | `session.rs` | PII hash 对齐 |
| 流消费 | `stream_consumer.py` (35KB) | `stream_consumer.rs` | 对齐 |
| MCP 配置 | — | `mcp_config.rs` | **Rust 新增** |
| 网络层 | `telegram_network.py` (9KB) | ❌ | 待移植 |
| Wecom 加密 | `wecom_crypto.py` (5KB) | ❌ | 待移植 |
| 帮助函数 | `helpers.py` (9KB) | ❌ | 待移植 |
| 状态报告 | `status.py` (15KB) | (在 gateway_mgmt.rs 中) | 对齐 |

### CLI

| 维度 | Python | Rust | 差异 |
|------|--------|------|------|
| CLI 框架 | `fire` (声明式) + argparse | `clap` (声明式) | 对齐 |
| 交互 TUI | `cli.py` (10033 行, prompt_toolkit) | `hermez-cli/tui/` + `app.rs` | Rust 在 hermez-cli 中 |
| 配置 | `hermez_cli/config.py` (143KB) | `hermez-cli/config_cmd.rs` + `hermez-core/config.rs` | Rust 拆分更合理 |
| 子命令 | `hermez_cli/main.py` (265KB, 集中式巨型文件) | `hermez-cli/` 31 个独立文件 | **Rust 更清晰** |
| 主文件 | `main.py` 265KB (所有子命令在一个文件) | 每个命令独立文件 (~200-400 行/文件) | Rust 维护友好 |

### State/Storage

| 维度 | Python | Rust | 差异 |
|------|--------|------|------|
| 会话存储 | `hermez_state.py` (1238 行) | `hermez-state/session_db.rs` | 对齐 |
| FTS5 搜索 | ✅ | ✅ | 对齐 |
| WAL 模式 | ✅ | ✅ | 对齐 |
| Schema | 内嵌 Python 字符串 | `schema.rs` BASE_SCHEMA_SQL | 对齐 |
| Insights 引擎 | `agent/insights.py` (34KB) | `hermez-state/insights.rs` | 对齐 |

### ACP (IDE 集成)

| 维度 | Python | Rust | 差异 |
|------|--------|------|------|
| 协议 | `acp_adapter/` (8 文件) | `hermez-acp/` (3 文件) | Rust 更精简 |
| JSON-RPC 方法 | 13+ | 13 | 对齐 |
| 会话管理 | `acp_adapter/session.py` | `hermez-acp/session.rs` | 对齐 |
| 协议类型 | `acp_adapter/protocol.py` | `hermez-acp/protocol.rs` | 对齐 |

## 3. 架构差异总结

### Rust 做得更好的

1. **模块拆分**: Python `run_agent.py` 11487 行单文件 → Rust 拆成 14 个子模块
2. **CLI 维护性**: Python `main.py` 265KB 单文件 → Rust 31 个独立命令文件
3. **依赖安全**: Rust 5 层严格 DAG, 编译期禁止循环依赖
4. **Tool Call 解析**: 10 个 provider 专用解析器 vs Python 内嵌在单文件
5. **注入防御**: `injection_scan.rs` 是 Rust 新增的安全特性
6. **Gateway 精简**: Python `run.py` 460KB → Rust 拆成 8 个合理模块

### Python 更完善的

1. **Gateway 平台覆盖**: 18 个适配器 vs Rust 的 5 个 — **最大差距**
2. **Bedrock 适配**: AWS Bedrock adapter 未移植
3. **测试覆盖**: 577 测试文件 vs Rust 的单元测试 + 48 E2E
4. **PTTY 终端**: ptyprocess/pywinpty 支持 (PTY 模式)
5. **网络库**: SOCKS proxy, Camofox browser 等

### 可优化项

1. **集中注册瓶颈**: `register_all_tools()` 需改为过程宏自动注册
2. **hermez-llm 过重**: 15 模块可拆出 `hermez-llm-providers`
3. **Gateway 适配缺失**: 13 个国内不需要的平台可不移植, 但 Telegram/Discord/Slack/WhatsApp 是高频平台
4. **测试 crate**: 缺少专用集成测试 crate

## 4. 移植完成度

| 模块 | Python 行数/大小 | Rust 行数 | 完成度 |
|------|-----------------|-----------|--------|
| Agent Engine | ~11487 行 (主文件) + 30 agent/ 文件 | 14 模块, ~2000 行 | **85%** |
| LLM 层 | openai + anthropic SDK | 15 模块, 含 10 provider 解析器 | **95%** |
| Tools | 59 文件 + 8 环境 | 55+ 文件 + 6 环境 | **90%** |
| Gateway | 25 文件 (18 适配器) | 8 文件 (5 适配器) | **45%** |
| CLI | 265KB main.py + 48 文件 | 31 独立命令文件 | **90%** |
| State | 1238 行 | 5 模块, ~800 行 | **95%** |
| Prompt | 47KB prompt_builder + 其他 | 9 模块, ~600 行 | **95%** |
| ACP | 8 文件 | 3 文件 | **95%** |
| **总体** | — | — | **~80%** |

**剩余工作量**: 主要集中在 Gateway 的 13 个缺失平台适配器和 Bedrock 适配。如果只考虑国内平台需求, 完成度已到 ~90%。
