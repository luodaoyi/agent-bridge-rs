# Worker and PTY Lifecycle

> 核心源码：[src/worker.rs](../../src/worker.rs)、[src/session.rs](../../src/session.rs)

## 职责与边界

- [src/worker.rs](../../src/worker.rs)：负责定位可执行文件（`GROK_BIN`/`CODEX_BIN` 覆写与 `PATH` 发现）、剥离父宿主环境变量、拼装 Grok/Codex 命令行参数、探测状态目录可写性、识别 TUI 阻塞界面和窗口标题阶段。
- [src/session.rs](../../src/session.rs)：管理 ConPTY / Unix PTY 会话、有界 VT100 屏幕快照与增量游标流、Hook 事件状态合并、租约超时回收及 Windows Job Object / Unix 进程组级联终止。

## 关键符号与行为契约

- **Windows 二进制与 `.cmd` shim 解析**（`resolve_worker_bin`、`parse_windows_node_cmd_shim`、`build_probe_command`）：
  - 当未设置 `CODEX_BIN` / `GROK_BIN` 时，Windows 按 `PATH` 目录顺位查找 `.exe` / `.cmd` / `.bat`，确保 pnpm 安装的新版 `codex.CMD` 优先于桌面端自带旧版 `codex.exe`。
  - 命中 npm/pnpm `.cmd` 包装脚本时直接解析为 `node.exe <script.js>` 离散参数启动，避免 `cmd.exe` 元字符破坏 Prompt；非 node shim 时回退 `cmd.exe /d /c`。
  - `doctor` 探活与 `create` 启动必须共用意图一致的 `build_probe_command` / `build_worker_command`。
- **Codex 参数映射**（`build_worker_command`）：
  - `--always-approve` 默认映射 `-s workspace-write -a never`（支持自定义 `--sandbox`）；
  - `--dangerously-bypass` 独占映射 `--dangerously-bypass-approvals-and-sandbox`。
  - 已知 Windows 运行时边界：部分 Windows 环境下 Codex `0.172+` 的 `-s workspace-write` 受限令牌会导致子命令报 `-1073741502`，此时需传 `--sandbox danger-full-access`。
- **快速退出与状态收敛**（`mark_exit`、`mark_reader_done`、`finalize_session`、`SessionRegistry::wait`）：
  - Windows ConPTY 下子进程自然快速退出时，`mark_exit` 须立即关闭 writer 并释放 `master` 句柄触发 `ClosePseudoConsole`，使 reader 排空剩余输出并到达 EOF；`WaitCondition::TuiIdle` 在 `process_done` 置位后即刻结束等待，避免无意义的 180 秒阻塞。
- **阻塞与标题启发式**（`blocked_reason`、`phase_from_title`）：
  - 识别 Codex / Grok 的目录信任提示、Hook 审查提示、审批弹窗及标题栏运行/空闲态。

## 验证命令

```sh
cargo test --locked worker::
cargo test --locked session::
```
