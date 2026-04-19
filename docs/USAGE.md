# Hermes Agent CLI 使用文档

> 由 Nous Research 构建的自进化 AI Agent

## 安装

### Windows

1. 下载 `hermes.exe` 到任意目录（如 `C:\hermes\`）
2. 将该目录加入 `PATH` 环境变量，或直接使用绝对路径

```powershell
# 验证安装
hermes --version
```

### 从源码构建

```bash
cd hermes-rs
cargo build --release
# 二进制位于 target/release/hermes (Linux/macOS) 或 hermes.exe (Windows)
```

---

## 快速开始

### 1. 首次配置

```bash
hermes setup
```

交互式引导会引导你完成：
- 选择 AI 模型提供商（OpenRouter / OpenAI / Anthropic 等）
- 输入 API Key
- 选择默认模型
- 配置工具集

配置文件存储在：
- `~/.hermes/config.yaml` — 设置
- `~/.hermes/.env` — API 密钥

### 2. 开始对话

```bash
hermes chat
```

或指定模型：

```bash
hermes chat -m anthropic/claude-sonnet-4-20250514
```

---

## 命令参考

### chat — 交互式对话

```bash
hermes chat [OPTIONS]

# 常用选项
-m, --model <MODEL>              指定模型（如 openrouter/openai/gpt-4o）
-q, --quiet                      安静模式（抑制调试输出）
-v, --verbose                    详细日志
    --skip-context-files         跳过加载上下文文件
    --skip-memory                跳过加载记忆
    --voice                      语音模式
```

### setup — 交互式配置向导

```bash
hermes setup
hermes setup model            # 仅配置模型
hermes setup --non-interactive
hermes setup --reset          # 重置为默认值
```

### gateway — 消息平台网关

支持 15+ 消息平台（Telegram、Discord、Slack、微信、飞书、钉钉、企业微信等）。

```bash
# 后台启动
hermes gateway start

# 前台运行（调试用）
hermes gateway run

# 停止
hermes gateway stop

# 查看状态
hermes gateway status

# 安装为系统服务（systemd/launchd/Windows Task Scheduler）
hermes gateway install

# 卸载服务
hermes gateway uninstall
```

### config — 配置管理

```bash
# 查看当前配置
hermes config show

# 编辑配置文件
hermes config edit

# 设置配置值
hermes config set agent.model anthropic/claude-sonnet-4-20250514
hermes config set compression.enabled true
hermes config set terminal.backend docker

# 查看配置文件路径
hermes config path
hermes config env-path

# 检查配置
hermes config check

# 迁移配置
hermes config migrate
```

### tools — 工具管理

```bash
# 列出所有可用工具
hermes tools list

# 查看工具详情
hermes tools info <tool-name>
```

### skills — 技能管理

```bash
# 列出所有技能
hermes skills list

# 查看技能详情
hermes skills info <skill-name>

# 启用/禁用
hermes skills enable <skill-name>
hermes skills disable <skill-name>

# 列出已注册的斜杠命令
hermes skills commands
```

### sessions — 会话管理

```bash
# 列出近期会话
hermes sessions list

# 按关键词搜索会话
hermes sessions search "query"

# 导出会话到 JSON
hermes sessions export --session-id <id> --output session.json

# 删除会话
hermes sessions delete --session-id <id>

# 重命名会话
hermes sessions rename --session-id <id> --new-name <name>

# 修剪旧会话
hermes sessions prune

# 会话统计
hermes sessions stats

# 浏览会话
hermes sessions browse
```

### profiles — 多 Profile 管理

```bash
# 列出所有 profile
hermes profiles list

# 创建新 profile
hermes profiles create <name>

# 切换到指定 profile
hermes profiles use <name>
```

所有命令都支持 `--hermes-home <path>` 临时指定数据目录。

### models — 列出可用模型

```bash
hermes models
```

### auth — 认证管理

```bash
# 添加 API Key
hermes auth add

# 列出已配置的密钥
hermes auth list

# 移除密钥
hermes auth remove <provider>

# 重置密钥
hermes auth reset

# 查看登录状态
hermes auth status
```

### login — OAuth 登录

```bash
hermes login <provider>     # google, anthropic, openai
hermes login google --no-browser
```

### logout — 清除凭据

```bash
hermes logout               # 清除所有提供商
hermes logout --provider openai
```

### status — 组件状态

```bash
hermes status
hermes status --deep        # 深度检查
hermes status --all         # 显示脱敏详情
```

### insights — 会话分析

```bash
hermes insights             # 默认最近 30 天
hermes insights --days 7    # 最近 7 天
hermes insights --source telegram
```

### backup — 备份状态

```bash
# 创建备份
hermes backup
hermes backup --quick       # 仅关键数据
hermes backup --label v1    # 添加标签

# 恢复备份
hermes restore /path/to/backup

# 从 zip 归档恢复
hermes import /path/to/backup.zip

# 列出可用备份
hermes backup-list
```

### logs — 查看日志

```bash
hermes logs                 # 最近 50 行 agent 日志
hermes logs agent -n 200    # 最近 200 行
hermes logs gateway         # 网关日志
hermes logs errors          # 错误日志
hermes logs --follow        # 实时跟踪
hermes logs --level warn    # 仅警告
hermes logs --since 1h      # 最近 1 小时以来的日志
```

### mcp — MCP 服务器管理

```bash
hermes mcp list
hermes mcp add <name> --url <server-url>
hermes mcp remove <name>
```

### memory — 外部记忆管理

```bash
hermes memory list
hermes memory set <provider>
```

### plugins — 插件管理

```bash
hermes plugins list
hermes plugins enable <plugin>
hermes plugins disable <plugin>
```

### webhook — Webhook 订阅

```bash
hermes webhook list
hermes webhook subscribe <url>
hermes webhook remove <id>
```

### model — 模型管理

```bash
hermes model                # 交互式选择模型
hermes model list           # 列出已配置模型
```

### update — 自我更新

```bash
hermes update
hermes update --preview     # 预发布版本
hermes update --force       # 强制更新
```

### uninstall — 卸载

```bash
hermes uninstall
hermes uninstall --keep-data --keep-config
```

### dashboard — 分析仪表板

```bash
hermes dashboard            # 默认 http://127.0.0.1:8080
hermes dashboard --port 3000
```

### debug / dump — 调试

```bash
hermes debug                # 打印调试信息
hermes debug-share          # 生成调试报告并分享
hermes dump                 # 转储会话数据
hermes dump <session-id> --show-keys
```

### completion — Shell 补全

```bash
hermes completion           # bash（默认）
hermes completion --shell zsh
hermes completion --shell fish
```

### acp — IDE 集成

```bash
hermes acp                  # Agent Client Protocol
hermes acp --editor vscode
```

### whatsapp — WhatsApp 配置

```bash
hermes whatsapp setup
hermes whatsapp status
```

### pairing — 设备配对

```bash
hermes pairing list
hermes pairing register
```

### version — 版本信息

```bash
hermes version
```

### claw — OpenClaw 迁移

从 OpenClaw（或 Clawdbot/Moltbot）迁移配置、记忆、技能到 Hermes。

```bash
# 预览迁移（默认，不修改文件）
hermes claw migrate --dry-run

# 执行迁移（确认后进行）
hermes claw migrate

# 完整迁移，跳过确认
hermes claw migrate --preset full --yes

# 仅迁移用户数据（不含 API 密钥）
hermes claw migrate --preset user-data

# 从自定义路径迁移
hermes claw migrate --source /path/to/.openclaw

# 清理迁移后的 OpenClaw 目录
hermes claw cleanup
```

注意：OpenClaw 迁移依赖 Python 3.11+ 和 `openclaw_to_hermes.py` 脚本。

### batch — 批量处理

```bash
# 处理 JSONL 数据集
hermes batch run --input data.jsonl --output results.jsonl

# 查看可用的工具集发行版
hermes batch distributions

# 查看批处理状态
hermes batch status

# 列出批处理任务
hermes batch list
```

### cron — 定时任务

```bash
# 列出已计划的任务
hermes cron list

# 创建定时任务
hermes cron create --cron "0 9 * * *" --prompt "每日晨报"

# 删除定时任务
hermes cron delete --id <job-id>

# 暂停/恢复
hermes cron pause --id <job-id>
hermes cron resume --id <job-id>

# 编辑任务
hermes cron edit --id <job-id>

# 手动执行任务
hermes cron run --id <job-id>

# 查看任务状态
hermes cron status --id <job-id>

# 移除任务
hermes cron remove --id <job-id>
```

### doctor — 诊断

```bash
hermes doctor
hermes doctor --fix         # 自动修复发现的问题
```

检查：
- 配置文件是否存在
- API 密钥是否有效
- 会话数据库是否正常
- 工具是否正确注册
- 常见配置错误

---

## 高级用法

### 环境变量

| 变量 | 说明 |
|------|------|
| `HERMES_HOME` | 自定义数据目录（替代 `~/.hermes`） |
| `OPENAI_API_KEY` | OpenAI API 密钥 |
| `ANTHROPIC_API_KEY` | Anthropic API 密钥 |
| `OPENROUTER_API_KEY` | OpenRouter API 密钥 |
| `DEEPSEEK_API_KEY` | DeepSeek API 密钥 |
| `GOOGLE_API_KEY` | Google/Gemini API 密钥 |

### 配置文件结构

`~/.hermes/config.yaml`:

```yaml
agent:
  model: anthropic/claude-sonnet-4-20250514
  provider: anthropic
  quiet: false
  toolsets:
    - filesystem
    - web
    - terminal

compression:
  enabled: true
  target_tokens: 50

terminal:
  backend: local
  docker_image: ubuntu:latest
```

### 多 Profile 隔离

每个 profile 有独立的数据目录：

```bash
hermes --hermes-home ~/.hermes-dev setup
hermes --hermes-home ~/.hermes-dev chat

hermes --hermes-home ~/.hermes-prod setup
hermes --hermes-home ~/.hermes-prod chat
```

---

## Gateway 平台支持

| 平台 | 适配器 | 备注 |
|------|--------|------|
| Telegram | telegram | Bot Token |
| Discord | discord | Bot Token |
| Slack | slack | Bot Token |
| 微信 | weixin | 微信公众号/个人号 |
| 飞书 | feishu | App ID + App Secret |
| 钉钉 | dingtalk | Client ID + Client Secret |
| 企业微信 | wecom | Corp ID + Agent ID |
| Signal | signal | signal-cli |
| WhatsApp | whatsapp | waha 网关 |
| 飞书(国际) | lark | Lark App ID |
| 飞书国内版 | feishu | Feishu Open API |
| OpenAI API | api_server | OpenAI 兼容 HTTP API |

启动指定平台：

```bash
hermes gateway run --platform telegram
```

---

## 数据目录结构

```
~/.hermes/
├── config.yaml              # 主配置
├── .env                     # API 密钥
├── sessions.db              # SQLite 会话数据库 (含 FTS5 搜索)
├── cron_jobs.json           # 定时任务
├── webhooks.json            # Webhook 订阅
├── .plugin_registry.json    # 插件注册表
├── skills/                  # 技能文件
│   ├── index.json
│   └── *.md
├── plugins/                 # 插件目录
└── logs/                    # 日志
```

---

## 常见问题

### Q: 如何切换模型？

```bash
hermes config set agent.model openrouter/openai/gpt-4o
```

或在对话中随时指定：

```
>m anthropic/claude-sonnet-4-20250514
```

### Q: 如何查看帮助？

```bash
hermes help
hermes chat --help
hermes gateway --help
```

### Q: 配置有问题怎么办？

```bash
hermes doctor
```

### Q: 如何清理旧会话？

```bash
hermes sessions delete --session-id <id>
```

### Q: 如何备份数据？

```bash
hermes backup create
hermes backup list
```
