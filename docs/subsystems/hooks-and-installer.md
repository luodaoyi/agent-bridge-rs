# Hooks and Dual-Host Installer

> 核心源码：[src/hooks.rs](../../src/hooks.rs)、[src/install.rs](../../src/install.rs)、[hooks/](../../hooks/windows/agent-bridge-codex.json)、[plugin.json](../../plugin.json)、[SKILL.md](../../SKILL.md)、[skills/codex-worker/SKILL.md](../../skills/codex-worker/SKILL.md)、[skills/grok-worker/SKILL.md](../../skills/grok-worker/SKILL.md)

## 职责与边界

- [src/hooks.rs](../../src/hooks.rs)：管理 Grok（14 事件、`AGENT_BRIDGE_HOOK_PROTOCOL="5"`）与 Codex（8 事件）的 Hook 注册、状态巡检、安全卸载及 `__hook` 标准输入事件解析转发。
- [src/install.rs](../../src/install.rs)：根据目标（`all` / `codex` / `agy`）部署 Codex Skill（`~/.agents/skills/agent-bridge/`）、Antigravity 原生插件（`~/.gemini/config/plugins/agent-bridge/`）与双 Worker Hooks，并执行字节级幂等更新比较。

## 核心不变量

1. **Codex `~/.codex/hooks.json` 无损合并**：
   - `install_at` / `uninstall_at` 只识别并增删命令体（含 Windows PowerShell UTF-16LE `-EncodedCommand` 解码后）包含 `__hook --worker codex` 的托管条目。
   - 用户原有条目（例如 `rtk hook codex`）必须原样保留；卸载仅移除本工具托管条目并在 matcher 空后清理空数组。
   - Codex Hook handler 不得写入额外 `"env"` 字段，确保兼容 Codex CLI schema 校验。
2. **旧版 `grok-bridge.json` 只提醒不删除**：
   - 仅检查 `$GROK_HOME/hooks/grok-bridge.json` 是否存在，在安装结果、TUI 提示与 `doctor` JSON（`legacy_grok_bridge_path`）中提醒用户手动删除，不自动删改。
3. **单一主 Hook 二进制口径**：
   - 当以 `InstallTarget::All` 或 `Agy` 安装时，`grok` 与 `codex` Hook 统一指向 `paths.agy_binary`；仅在单独 `InstallTarget::Codex` 时指向 `paths.codex_binary`。`status()` 与 `apply_from()` 保持完全一致。

## 验证命令

```sh
cargo test --locked hooks::
cargo test --locked install::
```
