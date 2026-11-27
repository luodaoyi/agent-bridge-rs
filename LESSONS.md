# LESSONS.md

> 借鉴 `yetone/magpie` 机制：记录每次真实返工、线上踩坑或实测边界及其防错规则。
> **晋升规则**：同一类教训在实战中累计出现 **3 次**，必须晋升为 [docs/code-standards.md](docs/code-standards.md) 中的硬规则，并补充对应的自动化守卫测试。

## 复盘与读取原则

- **实复现再修**：先按报障用户的精确操作系统、CLI 版本、宿主/执行器链路和原始输出构造复现测试（优先保存真实原始字节作为 fixture，不靠脑补样例）。
- **通读 Issue 到最后一条评论**：动工前检查最后一条评论是否有暂停、撤回或范围变更。
- **不以“仅本地过了”替代全量门禁**：`cargo fmt --check`、全量 `cargo test --all-targets --locked`、`cargo clippy --locked --all-targets --all-features -- -D warnings` 与 `node test/launcher.test.js` 必须全部在最终提交树上运行并通过。

---

## 历史踩坑记录（v0.9.0 基线）

### 1. Windows `PATH` 上 `.cmd` shim 优先于桌面端自带旧版 `codex.exe`（`3d101ec`）
- **现象**：用户通过 pnpm 全局安装的 `codex.CMD` 为 `0.172.0`，但 Desktop 应用附带的 `OpenAI\Codex\bin\codex.exe` 为 `0.144.1`。硬编码查找 `codex.exe` 绕过了 `PATH` 顺位，导致旧版二进制读 `~/.codex/config.toml` 时直接报 `invalid type: map, expected a boolean in features`。
- **规则**：Windows 默认解析 Worker 不得硬编码 `.exe` 扩展名；需按 `PATH` 目录顺序扫描 `.exe`/`.cmd`/`.bat`，并对 npm/pnpm `.cmd` 包装脚本直接解包为 `node.exe <script.js>` 启动（规避 `cmd.exe` 元字符破坏多行/符号 Prompt）。`doctor` 探测与 `create` 启动必须共享同一套命令拼装函数（`worker::build_probe_command`）。
- **Seen**: 1 (2026-11-26)
- **Status**: promoted
- **Guard**: `worker::tests::parses_pnpm_and_npm_windows_cmd_node_shims`, `worker::tests::build_worker_command_unwraps_cmd_shim_on_disk`

### 2. Windows 隔离宿主桌面（`WinSta0\exebox-*`）导致 `-s workspace-write` + `mxc` 子进程 `-1073741502`
- **现象**：从 Antigravity 终端启动 `agent-bridge` 并以 `codex`（`[windows] sandbox = "mxc"` + `-s workspace-write`）派发任务时，内部 PowerShell 与 `node_repl` kernel 全部异常退出 `STATUS_DLL_INIT_FAILED`（`-1073741502` / `0xC0000142`）；而在 `WinSta0\Default` 桌面或显式 `--sandbox danger-full-access` 下则正常执行。
- **根因**：Antigravity 的工具 shell 运行在受限隔离桌面 `WinSta0\exebox-*` 上，该桌面 DACL 将 `AC`（`ALL APPLICATION PACKAGES`）与 `S-1-15-2-2` 裁剪为仅 `CCDTCRRC`（缺失 `DESKTOP_WRITEOBJECTS` / `DESKTOP_CREATEWINDOW`）；若 `agent-bridge` 保持 `STARTUPINFOW.lpDesktop = NULL` 继承 `exebox-*`，或被拉起的 `codex`（`0.172.0+`）复用了先前运行在 `exebox-*` 上的全局单例 `app-server --managed-daemon`，都会使 `mxc` 沙盒内加载 `user32.dll` 时 `DllMain` 初始化失败。
- **规则**：Windows 下 `start_detached_server()` 与 `run_windows_job_child()` 在当前处于交互式窗口站 `WinSta0` 的非 `Default` 桌面且 `Default` 可打开时，自动以 `lpDesktop = "WinSta0\\Default"` 启动（非交互窗口站或打开失败时透明回退 `NULL`），且在探测到 `codex --help` 支持 `--no-daemon` 时自动附带 `--no-daemon` 避免复用残留的 `exebox-*` 共享 `app-server` 守护进程；`magpie-loop` 仍保持独占 `.worktrees/issue-<N>` 默认 `workspace-write` 沙盒配置。
- **Seen**: 2 (2026-11-26, 2026-11-27)
- **Status**: active
- **Guard**: `transport::tests`, `worker::tests::detects_no_daemon_support_from_codex_help_and_injects_flag_for_codex_only`

### 3. Codex `0.172.0` TUI `PasteBurst` 吞掉括号粘贴后紧跟的 `\r`
- **现象**：`create --prompt` 作为命令行参数可直接触发首轮任务，但多轮调用 `send --text` 时将 `\x1b[200~...\x1b[201~\r` 一次性写入 ConPTY，Codex `0.172.0` 终端组件把瞬时 `\r` 判入粘贴突发窗口而未触发提交（文本停留在输入框）。
- **规则**：Codex 高层 `send --text` 将括号粘贴与回车作为同一个队列项：粘贴体写入并 flush 后等待 200ms，再单独写入并 flush 回车，避免 PasteBurst 吞掉提交且不允许其他输入插队。两段字节必须合并计入准入预算；完成、失败及关闭取消均释放完整预算，writer 停止时也释放队列剩余预算。等待期间每约 20ms 检查关闭标志，取消时不得补写回车或记录错误。单字节 ASCII 控制输入、原始写入及 Grok 的单次写入保持原行为。在自动化无交互场景（如 `magpie-loop`）仍优先使用 `codex exec` stdin 模式。
- **Seen**: 1 (2026-11-26)
- **Status**: active
- **Guard**: `codex_send_separates_paste_from_submit`, `codex_send_admits_paste_and_submit_budget_together`, `codex_writer_flushes_paste_then_delayed_submit_without_interleaving`, `writer_shutdown_cancels_followup_and_releases_all_pending_bytes`, `writer_errors_release_paste_followup_and_queued_budgets`, `controls_raw_writes_and_grok_send_have_no_followup`

### 4. Windows ConPTY 快速失败退出必须释放 `master` 句柄（`3d101ec`）
- **现象**：子进程启动后秒退（`exit_code = 1`），`mark_exit` 设置了 `process_done = true`，但 ConPTY 读端管道未收到 EOF（`reader_done = false`），导致会话长期停在 `phase = running`、`wait --for tui-idle` 超时 180 秒。
- **规则**：Windows 下 `mark_exit` 若尚未进入终态，必须关闭 writer 并释放 PTY `master`（触发 `ClosePseudoConsole` 让 reader 排空缓冲并收到 EOF），同时 `wait` 检查到 `process_done` 即可终止等待。
- **Seen**: 1 (2026-11-26)
- **Status**: promoted
- **Guard**: `session::tests::windows_session_close_contains_descendant_after_intermediates_exit`

### 5. 本地工具链与 CI 工具链跨版本 `clippy` / `#[cfg]` 漂移（`e4d27ae`、`3d101ec`）
- **现象**：本地 Rust 1.95 零告警通过，但 GitHub Actions Ubuntu/Windows Runner 上的 Rust 1.100 对 `AtomicUsize::fetch_update`、`chunks_exact(2)`、`needless_bool` 及仅 Unix/Windows 下才未使用的 import 报错（`-D warnings`）。
- **规则**：新增/调整跨平台代码时确保 `#[cfg(windows)]` / `#[cfg(unix)]` 覆盖全部只读分支与 import；原子更新优先使用稳定便携原语（如 `compare_exchange_weak`）或标准块切片 API（`as_chunks`）；推送后务必等 6 平台 CI 全绿再打 Release Tag。
- **Seen**: 2 (2026-11-26, 2026-11-27)
- **Status**: promoted
- **Guard**: `docs/code-standards.md`, GitHub Actions 6-platform `clippy -D warnings`

### 6. CLI `codex` 版本间的审批参数兼容性
- **现象**：`codex-cli 0.144.1` 无 `--approve-for-me`，而 `0.144.1` 与 `0.172.0` 均支持 `-a never`（`--ask-for-approval never`）。
- **规则**：拼装 Worker CLI 参数优先使用多版本共同支持的稳定标志，并保持与实测本机版本互认。
- **Seen**: 1 (2026-11-26)
- **Status**: active
- **Guard**: `worker::tests::builds_codex_command_with_default_workspace_write_approval_and_strips_parent_env`

### 7. PR Closing Keyword 被反引号包裹导致 GitHub 不自动关闭 Issue（PR #59）
- **现象**：PR #59 正文中写成 `` `Fixes #36` ``，合并后 GitHub 未自动关闭 Issue #36，直到后续手动补充说明关闭。
- **规则**：PR 正文中联动关闭 Issue 的关键字（`Fixes #N` / `Closes #N`）禁止放在行内反引号或代码块中。
- **Seen**: 1 (2026-11-27)
- **Status**: active
- **Guard**: `scripts/check-governance.mjs` (`validateDiff` backtick closing-keyword check)

### 8. 全新 Worktree 缺少 `webui/dist` 导致纯后端修复无法编译（`include_bytes!`）
- **现象**：`webui/dist` 被 `.gitignore` 忽略而 `src/server.rs` 通过 `include_bytes!` 内嵌前端静态产物；新建 `.worktrees/*` 执行纯 Rust 修改时若不先构建 `webui/dist`，`cargo test` / `cargo build` 直接编译失败。
- **规则**：`magpie-loop` 校验链和手工工作树在缺少 `webui/dist/index.html` 时自动补建 `webui/dist`，无需每次手动排错。
- **Seen**: 2 (2026-11-27)
- **Status**: active
- **Guard**: `scripts/magpie-loop/execution.mjs` (`needsWebuiDistBootstrap`)

### 9. jsdom 跨 Realm `Uint8Array` 判定失效与 xterm 6.0.0 `?2026` 布尔同步状态（PR #61）
- **现象**：Vitest/jsdom 下 `TextEncoder.encode()` 产物与模块内全局 `Uint8Array` 属于不同 realm 导致 `instanceof Uint8Array` 返回 `false`；同时 xterm 6.0.0 将 DEC 2026 同步输出实现为布尔态而非嵌套计数器，剥离 `S+text+S+E` 末尾的 `S+E` 会遗留悬空 `S` 锁屏约 1 秒。
- **规则**：二进制流过滤用 `ArrayBuffer.isView()` 替代 `instanceof Uint8Array`；修改同步输出流必须追踪已透传 `inSync` 状态，仅在空闲态剔除空脉冲，已激活同步态时保留闭合 `SYNC_END`。
- **Seen**: 1 (2026-11-27)
- **Status**: active
- **Guard**: `webui/src/utils/terminalFilter.test.js`
