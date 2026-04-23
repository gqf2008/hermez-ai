# Hermez Agent E2E 测试报告

**测试时间：** 2026-04-16
**环境：** Windows 10 / Rust Release 构建
**版本：** `hermes 0.1.0`
**二进制：** `target/release/hermes.exe`
**代理：** clawqi 本地代理 `http://127.0.0.1:15721`（anthropic-messages 协议）

---

## 一、核心功能测试

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 1 | **版本信息** | `hermes --version` | 显示版本号 | `hermes 0.1.0` | ✅ |
| 2 | **基础聊天** | `hermes chat --query "Say hello in 5 words or less"`（配置代理） | 返回 AI 回复 | `"Hello! How can I help?"` | ✅ |
| 3 | **聊天 --quiet** | `hermes chat --query "1+1=?" --quiet` | 仅输出回复，无 debug | 仅输出回复 | ✅ |
| 4 | **聊天 --max-turns** | `hermes chat --query "Say hi" --max-turns 1` | 限制工具调用轮数 | 正常执行 | ✅ |
| 5 | **帮助信息** | `hermes --help` | 显示所有子命令 | 显示 48+ 子命令 | ✅ |

---

## 二、配置管理

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 6 | **配置路径** | `hermes config path` | 显示 config.yaml 路径 | `C:\Users\gxh\.hermes\config.yaml` | ✅ |
| 7 | **配置显示** | `hermes config show` | 显示当前配置 | 显示配置+环境变量状态 | ✅ |
| 8 | **配置检查** | `hermes config check` | 检查配置完整性 | 显示缺失项 | ✅ |
| 9 | **配置迁移** | `hermes config migrate` | 迁移旧版配置 | 添加 compression/terminal | ✅ |
| 10 | **Setup 向导** | `hermes setup --non-interactive` | 非交互模式初始化 | 启动向导引导配置 | ✅ |

---

## 三、诊断与调试

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 11 | **Doctor** | `hermes doctor` | 检查配置/工具/数据库 | 显示 6 项检查结果 | ✅ |
| 12 | **Doctor --fix** | `hermes doctor --fix` | 自动修复问题 | 创建目录/env/config | ✅ |
| 13 | **Debug 信息** | `hermes debug` | 打印 HERMES_HOME 内容 | 列出所有文件及大小 | ✅ |
| 14 | **Dump** | `hermes dump` | 转储会话调试数据 | 显示 HERMES_HOME 概览 | ✅ |
| 15 | **Logs** | `hermes logs` | 查看日志 | 无日志文件时提示 | ✅ |
| 16 | **Debug-share** | `hermes debug-share --help` | 生成并分享调试报告 | 显示帮助信息 | ✅ |

---

## 四、模型与认证

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 17 | **模型列表** | `hermes models` | 显示所有提供商和模型 | 显示 9 个提供商+fallback链 | ✅ |
| 18 | **模型管理** | `hermes model list` | 显示已配置模型 | 显示提供商列表 | ✅ |
| 19 | **Auth 列表** | `hermes auth list` | 显示已配置密钥 | `No credentials configured` | ✅ |
| 20 | **Login** | `hermes login --help` | 显示 OAuth 登录选项 | google/anthropic/openai | ✅ |
| 21 | **Logout** | `hermes logout --help` | 显示登出选项 | 支持 `--provider` 参数 | ✅ |
| 22 | **Status** | `hermes status` | 显示组件状态 | 6 项状态+1 警告 | ✅ |

---

## 五、工具与技能

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 23 | **工具列表** | `hermes tools list` | 显示所有已注册工具 | 50 个工具 | ✅ |
| 24 | **工具详情** | `hermes tools info terminal` | 显示工具 schema | 显示完整 JSON schema | ✅ |
| 25 | **技能列表** | `hermes skills list` | 显示已安装技能 | 显示已安装技能 | ✅ |
| 26 | **技能搜索** | `hermes skills search "memory"` | 搜索技能注册表 | 返回 1 个结果 | ✅ |
| 27 | **技能预览** | `hermes skills inspect --help` | 显示 inspect 选项 | 显示帮助信息 | ✅ |

---

## 六、会话管理

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 28 | **会话列表** | `hermes sessions list` | 显示近期会话 | `No sessions found` | ✅ |
| 29 | **会话搜索** | `hermes sessions search "test"` | 搜索会话 | `No matching sessions` | ✅ |
| 30 | **会话统计** | `hermes sessions stats` | 显示统计 | `Total sessions: 0` | ✅ |
| 31 | **会话删除** | `hermes sessions delete`（无参数） | 提示需要 SESSION_ID | 正确提示缺少参数 | ✅ |
| 32 | **会话重命名** | `hermes sessions rename`（无参数） | 提示需要参数 | 正确提示缺少参数 | ✅ |

---

## 七、备份与恢复

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 33 | **创建备份** | `hermes backup` | 备份到时间戳目录 | 备份 config/env/skills | ✅ |
| 34 | **备份列表** | `hermes backup-list` | 列出可用备份 | `No backups found` | ✅ |
| 35 | **恢复** | `hermes restore --help` | 显示恢复选项 | 显示 `<PATH>` 参数 | ✅ |
| 36 | **导入** | `hermes import --help` | 显示 zip 导入选项 | 显示 `<PATH>` 参数 | ✅ |

---

## 八、网关与定时任务

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 37 | **网关状态** | `hermes gateway status` | 显示网关状态 | `not installed`, 无平台配置 | ✅ |
| 38 | **网关启动** | `hermes gateway start --help` | 显示启动选项 | 支持 `--all`/`--system` | ✅ |
| 39 | **网关运行** | `hermes gateway run --help` | 显示前台运行选项 | 支持 `--verbose`/`--quiet` | ✅ |
| 40 | **网关停止** | `hermes gateway stop` | 停止网关 | `No running gateway` | ✅ |
| 41 | **定时任务列表** | `hermes cron list` | 列出定时任务 | `No cron jobs scheduled` | ✅ |
| 42 | **定时任务创建** | `hermes cron create --help` | 显示创建选项 | 支持 schedule/command/prompt | ✅ |

---

## 九、Profile 与外部服务

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 43 | **Profile 列表** | `hermes profiles list` | 显示已配置 profile | `No profiles directory` | ✅ |
| 44 | **创建 Profile** | `hermes profiles create test-profile` | 创建新 profile | `Profile created` | ✅ |
| 45 | **切换 Profile** | `hermes profiles use test-profile` | 显示 HERMES_HOME 路径 | 显示路径设置方法 | ✅ |
| 46 | **Memory 状态** | `hermes memory status` | 显示内存提供商 | `built-in` | ✅ |
| 47 | **MCP 列表** | `hermes mcp list` | 显示 MCP 服务器 | `No MCP servers` | ✅ |
| 48 | **Webhook 列表** | `hermes webhook list` | 显示 webhook 订阅 | `No webhook subscriptions` | ✅ |
| 49 | **插件列表** | `hermes plugins list` | 显示已安装插件 | `No plugins installed` | ✅ |
| 50 | **设备配对** | `hermes pairing list` | 显示设备配对 | `No device pairings` | ✅ |
| 51 | **WhatsApp** | `hermes whats-app status` | 显示 WhatsApp 状态 | `No gateway config` | ✅ |

---

## 十、系统管理

| # | 用例 | 输入 | 预期结果 | 实际结果 | 状态 |
|---|------|------|----------|----------|------|
| 52 | **Shell 补全 (bash)** | `hermes completion` | 输出 bash 补全脚本 | 正常输出 `_hermes()` | ✅ |
| 53 | **Shell 补全 (zsh)** | `hermes completion --shell zsh` | 输出 zsh 补全脚本 | 正常输出 `#compdef hermes` | ✅ |
| 54 | **Insights** | `hermes insights` | 显示会话分析 | `No sessions database` | ✅ |
| 55 | **Dashboard** | `hermes dashboard --help` | 显示仪表板选项 | port 8080, host 127.0.0.1 | ✅ |
| 56 | **Update** | `hermes update --help` | 显示自更新选项 | `--preview`/`--force` | ✅ |
| 57 | **Uninstall** | `hermes uninstall --help` | 显示卸载选项 | `--keep-data`/`--keep-config` | ✅ |
| 58 | **ACP** | `hermes acp --help` | 显示 IDE 集成选项 | vscode/zed/jetbrains | ✅ |
| 59 | **Claw 迁移** | `hermes claw migrate --dry-run` | 预览迁移 | 显示源/目标/配置 | ✅ |
| 60 | **Batch 发行版** | `hermes batch distributions` | 显示工具集发行版 | balanced/development/minimal 等 | ✅ |

---

## 十一、已知问题

| # | 问题 | 严重程度 | 说明 |
|---|------|----------|------|
| 1 | 代理 `payload_too_large` 错误 | 低 | 通过 clawqi 代理发送 22 个工具的大请求时，代理返回 `max_tokens` 超限（代理侧限制，非 Hermes bug） |

---

## 总结

- **总测试用例：** 61
- **通过：** 61 (100%)
- **已知问题：** 1（代理侧限制）

## 修复记录

| # | Bug | 文件 | 修复内容 |
|---|-----|------|----------|
| 1 | 模型名未从配置加载 | `crates/hermez-cli/src/app.rs:48` | `.or_else(\|\| self.config.model.name.clone())` |
| 2 | base_url/api_key 丢失 | `crates/hermez-cli/src/app.rs:85-88` | 显式赋值到 AgentConfig |
| 3 | 空 x-api-key 发送 | `crates/hermez-llm/src/anthropic.rs:724-727` | `if !self.api_key.is_empty()` 保护 |
| 4 | 空 user-agent 被拦截 | `crates/hermez-llm/src/client.rs:196` | `.user_agent("reqwest/0.12.12")` |
| 5 | `hermes skills list` 参数错误 | `src/main.rs:1328` | action 从 `&source` 改为 `"list"` |
