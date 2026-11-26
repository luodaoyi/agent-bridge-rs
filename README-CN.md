# agent-bridge 本地 Runtime（支持 Antigravity 与 Codex）

[English](README.md) | [简体中文](README-CN.md)

`agent-bridge` 是一个轻量级本地单例 PTY 运行时，支持 **2×2 有界宿主/执行器矩阵**：

- **Codex → Grok**（Codex 规划与终验，Grok 执行实现）
- **AGY → Codex**（Antigravity 规划路线与验收门禁，Codex 在指定 Worktree 编写代码与自测，AGY 独立看 Diff/跑测试并发起对抗式挑战打回）
- **AGY → Grok**（Antigravity 规划与挑战，Grok 执行实现）

运行时与旧版 `grok-bridge` 完全隔离（独立命名管道 `agent-bridge-runtime-v1-*`、默认 WebUI 端口 `127.0.0.1:47654`、独立环境变量前缀 `AGENT_BRIDGE_*`）。

## 快速开始

### 1. 环境准备

确认本机已安装并登录目标执行器 CLI（`codex` 和/或 `grok`）：

```bash
codex --version
grok --version
```

### 2. 安装与双宿主部署

```bash
pnpm add -g agent-bridge-rs@latest
# 或源码构建: cargo build --release
agent-bridge install
```

`agent-bridge install` 会自动部署：
- **Codex Skill**：`~/.agents/skills/agent-bridge/`（`SKILL.md`、`agents/openai.yaml`、原生二进制）
- **Antigravity (AGY) 原生插件**：`~/.gemini/config/plugins/agent-bridge/`（`plugin.json`、`skills/codex-worker/SKILL.md`、`skills/grok-worker/SKILL.md`、原生二进制）
- **Lifecycle Hooks**：
  - Grok Hooks → `~/.grok/hooks/agent-bridge.json`（若检测到旧版 `grok-bridge.json` 会提示你手动删除，不会自动擅删）
  - Codex Hooks → **无损合并**进 `~/.codex/hooks.json`（保留用户已有的第三方 Hook，如 `rtk hook codex`）

### 3. 常用命令

`create` 和创建态 `terminal` 必须显式指定 `--worker grok|codex`：

```bash
# AGY 调度 Codex（--always-approve 默认映射为 -s workspace-write -a never）
agent-bridge create --worker codex --cwd D:\repo\.worktrees\task\core --owner "任务标题" --prompt "Objective: ..." --always-approve

# 调度 Grok
agent-bridge create --worker grok --cwd D:\repo --owner "任务标题" --prompt "Objective: ..." --always-approve

# 等待空闲、读取输出、对抗式挑战打回、清理
agent-bridge wait --session <id> --for tui-idle --timeout-ms 300000
agent-bridge read --session <id> --cursor 0 --limit 4096 --wait-ms 5000
agent-bridge send --session <id> --text "Challenge round 1: ..."
agent-bridge close --session <id>
agent-bridge close-client   # 一键关闭当前宿主对话（CODEX_THREAD_ID / ANTIGRAVITY_CONVERSATION_ID）派生的全部子进程

# WebUI 看板与人工接管终端
agent-bridge server ui     # http://127.0.0.1:47654
agent-bridge terminal --session <id>
agent-bridge doctor --worker all
```

## 主要环境变量

| 变量 | 说明 |
| --- | --- |
| `CODEX_BIN` / `GROK_BIN` | 覆盖默认执行器可执行文件路径 |
| `AGENT_BRIDGE_WEB_ADDR` | WebUI 回环监听地址（默认 `127.0.0.1:47654`） |
| `AGENT_BRIDGE_ALLOWED_ROOTS` | 限制允许的 `--cwd` 根目录 |
| `AGENT_BRIDGE_CLIENT_LEASE_SECONDS` | 宿主租约超时秒数（默认 `120`） |
| `AGENT_BRIDGE_ORPHAN_GRACE_SECONDS` | 空闲孤儿宽限期秒数（默认 `600`） |
| `AGENT_BRIDGE_CJK_FONT` / `AGENT_BRIDGE_CJK_FONT_INDEX` | 覆盖 egui 中文字体回退路径 |
| `AGENT_BRIDGE_DISABLE_UPDATE_CHECK` | 禁用 GitHub Release 更新检查 |
