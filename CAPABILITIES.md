# Hermez AI 功能汇总

> 基于代码库当前状态（2026-04-23）

## 一、项目架构（Workspace）

依据：`Cargo.toml` 及 `crates/` 目录结构

| Crate | 职责 |
|-------|------|
| `hermez-core` | 基础类型、错误处理、Hermes home 路径等 |
| `hermez-llm` | LLM 调用层（OpenAI/Anthropic 等） |
| `hermez-prompt` | 提示词模板、系统提示管理 |
| `hermez-tools` | **工具注册表 + 工具实现 + 技能系统** |
| `hermez-agent-engine` | **Agent 核心引擎**（对话循环、工具调用、记忆、上下文压缩） |
| `hermez-gateway` | **多平台消息网关**（接收/发送消息） |
| `hermez-cli` | CLI 入口 + Gateway 启动器 |

---

## 二、Agent 引擎核心能力

依据：`crates/hermez-agent-engine/src/agent.rs`、`agent/types.rs`

### 2.1 对话管理
- **多轮对话循环**：`run_conversation()` 驱动对话，支持用户消息 -> LLM -> 工具调用 -> 结果反馈的完整循环
- **最大迭代次数控制**：默认 90 轮，可配置 `max_iterations`
- **系统提示词**：支持动态系统提示 + `ephemeral_system_prompt` 覆盖
- **上下文压缩**：当对话过长时自动压缩历史消息（`compression_enabled`）

### 2.2 工具调用（Tool Use）
- **工具去重**：同一轮中重复的工具调用会被去重
- **循环检测**：同一工具+参数连续调用 >=5 次自动中断，返回循环警告
- **并行执行**：独立工具批次并行执行，交互式/依赖工具串行执行
- **工具调用历史上限**：超过 30 条时自动清理仅出现一次的记录

### 2.3 模型支持
- **Provider 抽象**：支持 OpenAI、Anthropic、自定义 provider
- **API 模式切换**：`api_mode` 字段支持 `openai`、`anthropic_messages` 等
- **Tool Use Enforcement**：`ToolUseEnforcement::Auto` 对 Claude 模型跳过强制工具使用

### 2.4 记忆与进化
- **记忆提示**：`memory_nudge_interval`（默认 5 轮）触发记忆回顾
- **技能提示**：`skill_nudge_interval`（默认 5 轮）触发技能提醒
- **自我进化**：`self_evolution_enabled` 支持 Agent 自我改进
- **会话持久化**：`persist_session` + `session_db` 支持会话保存

### 2.5 回调系统
- **流式输出回调**：`set_stream_callback`（实时 content delta）
- **推理流回调**：`set_reasoning_stream_callback`（thinking/reasoning 内容）
- **工具开始回调**：`set_tool_gen_started_callback`（工具开始生成时通知）
- **状态回调**：`set_status_callback`（Agent 状态变化）

---

## 三、工具系统（Tools）

依据：`crates/hermez-tools/src/registry.rs`、`crates/hermez-tools/src/lib.rs`

### 3.1 工具注册机制
- `ToolRegistry` 统一管理所有工具
- `register()` / `deregister()` 动态注册
- `get_definitions()` 返回 LLM 可用的工具描述（JSON Schema）
- `get_available_tools()` 返回实际可调用的工具列表
- 每个工具可配置 `check_fn` 用于运行时可用性判断

### 3.2 内置工具

| 工具类别 | 具体工具 | 说明 |
|---------|---------|------|
| **定时任务** | `cronjob` | 创建/列出/删除定时任务（依据：`crates/hermez-tools/src/cron_tools.rs`） |
| **终端** | `terminal` | 执行 shell 命令（危险命令需 `check_dangerous_command` 审批） |
| **消息发送** | `send_message` | 向各平台发送消息 |
| **记忆** | `memory` | 长期记忆读写 |
| **技能** | `skills_list`、`skill_view`、`skill_install`、`skill_uninstall`、`skill_search`、`skill_manage`、`skill_taps` 等 | 技能管理 |
| **子 Agent** | `delegate_task` | 创建子 Agent 处理子任务（依据：`delegate.rs`） |
| **审批** | `check_dangerous_command` | 危险操作需要用户审批 |
| **文件操作** | `read_file`、`write_file`、`patch`、`search_files` | 读写/搜索文件 |
| **浏览器** | `browser_navigate`、`browser_click`、`browser_type`、`browser_press`、`browser_scroll`、`browser_snapshot`、`browser_get_images`、`browser_back`、`browser_console`、`browser_vision` | 浏览器自动化 |
| **图像** | `image_generate` | 图像生成 |
| **语音** | `text_to_speech`、`transcribe_audio` | TTS / 语音识别 |
| **HomeAssistant** | `ha_list_entities`、`ha_get_state`、`ha_list_services`、`ha_call_service` | Home Assistant 设备控制 |
| **网页** | `web_search`、`web_extract`、`web_crawl` | 搜索/提取/爬取网页 |
| **代码执行** | `execute_code` | 代码执行（沙箱环境） |
| **会话** | `session_search` | 历史会话搜索 |
| **视觉** | `vision_analyze` | 图像分析 |
| **MCP** | `mcp_client` | MCP 客户端工具 |
| **训练** | `rl_training` | RL 训练环境管理 |
| **其他** | `clarify`、`fuzzy_match`、`process`、`code`、`todo`、`math` | 澄清/模糊匹配/进程管理/代码/待办/数学 |

> 注：每个工具可配置 `requires_env` 字段，用于运行时根据环境变量可用性过滤工具列表。

---

## 四、技能系统（Skills）

依据：`crates/hermez-tools/src/skills/mod.rs`

### 4.1 技能加载
- 从 `~/.hermez/skills/` 目录加载技能
- 支持外部技能目录配置
- 技能文件使用 frontmatter（YAML）描述元数据

### 4.2 技能元数据
- `name`、`description`、`version`、`author`
- `tools`：技能依赖的工具列表
- `triggers`：触发条件
- `disabled` 状态管理

### 4.3 技能使用
- 运行时注入技能提示词到对话上下文
- `skill_nudge_interval` 控制技能提醒频率

---

## 五、消息网关（Gateway Platforms）

依据：`crates/hermez-gateway/src/runner.rs`、`platforms/`

### 5.1 支持的平台

| 平台 | 状态 | 关键特性 |
|-----|------|---------|
| **Feishu/Lark（飞书）** | 完整支持 | WebSocket + Webhook、流式回复（progressive card）、@提及过滤、群策略、允许列表 |
| **Weixin（微信）** | 支持 | 依据 `runner.rs` 中的 stale session cleanup |
| **Telegram** | 支持 | HTTP proxy 自动检测、fallback client |
| **WhatsApp** | 支持 | 依据 `runner.rs` |
| **Email** | 支持 | 依据 `runner.rs` |
| **QQ Bot** | 支持 | 依据 `platforms/qqbot.rs` |
| **DingTalk（钉钉）** | 支持 | `truncate_text` 复用共享工具 |
| **WeCom（企业微信）** | 支持 | `truncate_text` 复用共享工具 |
| **Matrix** | 支持 | 依据测试输出 |
| **Mattermost** | 支持 | 依据测试输出 |
| **Signal** | 支持 | 依据测试输出 |
| **HomeAssistant** | 支持 | 状态变更通知 |

### 5.2 Gateway 通用能力
- **消息去重**：5 分钟 TTL 去重（`MessageDeduplicator`，默认 2000 条目上限）
- **Stale Session 清理**：5 分钟超时，所有平台统一清理
- **审批系统**：`ApprovalRegistry` 用于危险操作的用户确认
- **允许列表**：按用户/群组配置访问控制
- **媒体缓存**：图片/文件/音频的本地缓存

---

## 六、CLI 能力

依据：`crates/hermez-cli/src/app.rs`

### 6.1 运行模式
- **交互模式**：REPL 对话
- **单次查询模式**：`--query` 参数直接提问
- **Gateway 模式**：启动消息网关服务器

### 6.2 Gateway 启动时的 Agent 配置
- 自动将 gateway config 中的 `provider`、`base_url`、`api_key`、`api_mode` 注入 Agent
- 流式回调桥接：`tokio::sync::mpsc` 将同步回调转为异步事件流
- Feishu 流式模式支持：progressive card 更新

---

## 七、配置系统

配置文件：`~/.hermez/config.yaml`

```yaml
_config_version: 18

model:
  name: claude-sonnet-4-20250514
  provider: custom
  base_url: http://127.0.0.1:15721
  api_mode: anthropic_messages

platforms:
  feishu:
    enabled: true
    extra:
      app_id: cli_xxx
      app_secret: xxx
      dm_policy: open
      group_policy: allowlist
      stream_mode: partial

terminal:
  backend: local
  max_output_size: 100000

memory:
  enabled: true

cron:
  enabled: true

plugins:
  auto_load: true
  dirs:
    - ~/.hermez/plugins
```

---

## 八、架构图

```
+-------------------------------------------------------------+
|                        hermez-cli                            |
|         (交互模式 / 单次查询 / Gateway 服务器)                |
+------------------------|------------------------------------+
                         |
+------------------------v------------------------------------+
|                   hermez-gateway                             |
|  Feishu | 微信 | Telegram | WhatsApp | Email | QQ | 钉钉... |
|  消息路由  去重  审批  会话清理  媒体缓存                      |
+------------------------|------------------------------------+
                         |
+------------------------v------------------------------------+
|                  hermez-agent-engine                         |
|  多轮对话  工具调用  循环检测  上下文压缩                       |
|  记忆系统  自我进化  子 Agent 委派                            |
+------------------------|------------------------------------+
                         |
+------------------------v------------------------------------+
|                    hermez-tools                              |
|  工具注册表: cron | terminal | send_message | memory | skills |
|  技能系统: 从 ~/.hermez/skills/ 加载                         |
+------------------------|------------------------------------+
                         |
+------------------------v------------------------------------+
|                     hermez-llm                               |
|           OpenAI / Anthropic / 自定义 Provider               |
+-------------------------------------------------------------+
```