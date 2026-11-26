# Subsystem Design References

> 本目录是 `agent-bridge-rs` 的轻量级架构地图。每篇只写**职责边界、状态转换、核心不变量、关键符号与验证命令**，不抄实现代码，不使用易过期的行号锚。

## 子系统索引

| 子系统 | 文档 | 核心源码 |
| --- | --- | --- |
| 协议、帧校验与宿主租约 | [protocol-and-transport.md](protocol-and-transport.md) | [src/protocol.rs](../../src/protocol.rs)、[src/transport.rs](../../src/transport.rs) |
| 执行器命令与 PTY 会话 | [worker-and-pty.md](worker-and-pty.md) | [src/worker.rs](../../src/worker.rs)、[src/session.rs](../../src/session.rs) |
| Hooks 与双宿主安装器 | [hooks-and-installer.md](hooks-and-installer.md) | [src/hooks.rs](../../src/hooks.rs)、[src/install.rs](../../src/install.rs)、[skills/](../../skills/) |
| Daemon、WebUI 与 GUI 终端 | [server-and-webui.md](server-and-webui.md) | [src/server.rs](../../src/server.rs)、[src/terminal_gui.rs](../../src/terminal_gui.rs)、[webui/](../../webui/) |
| 自动修复 Loop 与发版流程 | [loop-and-release.md](loop-and-release.md) | [scripts/](../../scripts/)、[.github/workflows/](../../.github/workflows/) |

## 文档同步维护契约

1. **契约变更随同 PR 更新**：当修改改变了上述任一子系统的职责边界、CLI/RPC 字段、状态机转换、兼容性规则或失败行为时，应在同一个 PR 内更新对应的子系统文档；纯内部重构且外部行为不变时无需修改文档。
2. **单一事实源**：每项规则只在一个地方定义，其余页面用相对路径链接引用，避免两处抄写产生分叉。
3. **PR 语义变更说明**：提交 PR 时需简要说明：
   - 触发条件与受影响子系统；
   - Before / After 行为差异；
   - 实际运行的验证命令与对应的回归测试名。

## 校验方式

运行 `node scripts/check-governance.mjs` 可自动校验本目录及 [docs/code-standards.md](../code-standards.md) 中的相对链接有效性，以及是否意外引入行号锚。
