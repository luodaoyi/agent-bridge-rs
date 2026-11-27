# Code Standards & Engineering Discipline

> 本文件汇总 `agent-bridge-rs` 的硬性编码标准、测试纪律与提交规范。与工作流分工参见 [AGENTS.md](../AGENTS.md)，历史踩坑参见 [LESSONS.md](../LESSONS.md)，子系统契约索引参见 [docs/subsystems/README.md](subsystems/README.md)。

## 1. Rust、IPC 与 ConPTY 规范

1. **禁止生产路径 `unwrap()` / `expect()`**：错误统一返回 `anyhow::Result` 并附上下文（`.context(...)`）；`unwrap()` 仅允许出现在 `#[cfg(test)]` 测试模块。
2. **禁止 Shell 字符串拼接启动外部进程**：无论 `CommandBuilder`（PTY）还是 `std::process::Command`（`doctor` / Windows shim），必须逐参数传递离散 `arg`；Windows `.cmd`/`.bat` npm/pnpm 包装脚本优先通过 `parse_windows_node_cmd_shim` 解包为 `node.exe <script.js>`。
3. **跨平台 `#[cfg]` 干净编译**：凡 Windows/Unix 独占的辅助函数、字段或 `use` 导入，必须在声明处精确标注 `#[cfg(windows)]` 或 `#[cfg(unix)]`，避免在另一平台触发 `clippy -D warnings` 下的 `dead_code` / `unused_imports`。
4. **可移植原子与切片原语**：优先使用跨 Rust 次版本稳定的 API（例如用 `compare_exchange_weak` 循环替代易受版本变更影响的原子辅助方法，用 `as_chunks` 替代固定大小 `chunks_exact`）。
5. **ConPTY 与底层终端边界**：
   - `send --text` 保留高层语义，不得擅自吞掉多行/特殊控制行为；
   - `write --data-base64` 必须逐字节原始写入，禁止隐式编码转换或追加换行；
   - `cwd` 必须经过 canonicalize 与 `AGENT_BRIDGE_ALLOWED_ROOTS` 校验。

## 2. Hook 与安装器铁律

1. **非破坏性合并 `~/.codex/hooks.json`**：安装与卸载 Codex Hooks 时只能精准增删含 `agent-bridge` 标识的条目，严禁覆盖第三方 Hook（如 `rtk hook codex`），且不在 Codex handler 中写入额外 `"env"` 字段。
2. **旧版 `grok-bridge.json` 只提醒不擅删**：检测到遗留 `$GROK_HOME/hooks/grok-bridge.json` 时只输出路径提醒供用户手动处理，禁止自动删除或迁改内容。
3. **主二进制路径一致**：`install`（默认 `All` 或 `Agy` 目标）须让 Grok 与 Codex 两份 Hook 指向同一个可验证二进制路径，使 `doctor --worker all` 与 `status install` 保持自洽。

## 3. WebUI 与多语言规范

1. **全部可见文本进 i18n**：新增或修改 WebUI 文案时须同步维护 `webui/src/i18n/catalog/` 下全部 15 个语言目录，确保 `webui` 单测通过。
2. **终端默认只读**：WebUI 终端默认保持只读观测；只读态与视口自动同步会发送 `terminal_resize`，但绝不刷新宿主 CLI 租约；仅在用户显式打开交互开关并发送 `terminal_input` 时才写入按键并刷新租约，且服务端严格绑定 loopback。
3. **内嵌构建同步**：修改 `webui/` 后必须在提交前运行 `npm test && npm run build`，保证本地与 CI 嵌入最新产物。

## 4. 测试与真实复现纪律

1. **禁止自动化测试触网或调真实 Agent**：Rust 与 Node 单元测试只能在隔离临时目录（`tempfile::tempdir()`、`ShortTempDir` 等）运行，不调用真实 `grok`/`codex`、不修改 `~/.codex/` / `~/.grok/` / `~/.agents/` / `~/.gemini/`。
2. **用真实报障字节做回归测试**：修复协议、阻塞检测、ANSI 屏幕或命令行参数 Bug 时，先写出“无此次改动前必失败”的回归测试，fixture 来自真实报错输出而非凭空捏造。
3. **跑全量，不报偏科结果**：每次提交前在最终工作树跑通 `cargo fmt --check`、`cargo test --all-targets --locked`、`cargo clippy --all-targets --all-features --locked -- -D warnings`、`node test/launcher.test.js`、`node scripts/check-governance.mjs` 与 `node --test test/governance.test.js`。

## 5. 文档、Commit 与 Worktree 纪律

1. **文档禁行号锚**：`docs/subsystems/` 与本文件引用代码只写文件路径或符号名（如 `src/worker.rs`、`resolve_worker_bin`），禁止在文件路径后附加冒号行号或 L 片段行号锚，以免代码微调后文档失真。
2. **一提交一目标，独占 Worktree**：自动 Loop 或并行任务必须在 `.worktrees/<task>` 独立分支作业；Commit Body 写清受影响子系统、实跑验证命令与回归测试名称。
3. **生产源码文件上限**：`src/`（非测试模块）与 `scripts/` 下所有生产源码文件不得超过 **600 行**（由 `scripts/check-governance.mjs` 的 `validateProductionLimits` 自动阻断）。
4. **不留无效状态**：`git status --short` 不得遗留临时日志、未跟踪补丁或无关文件的修改。
