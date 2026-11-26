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
- **守卫测试**：`worker::tests::parses_pnpm_and_npm_windows_cmd_node_shims`、`worker::tests::build_worker_command_unwraps_cmd_shim_on_disk`。

### 2. Windows 受限令牌沙箱 `-s workspace-write` 触发子进程 `-1073741502`
- **现象**：在 Windows 本机实测 `AGY -> Codex`（`codex-cli 0.172.0`）时，`-s workspace-write` 启用 Windows 受限令牌沙箱后，内部 PowerShell 与 Node 子进程全部异常退出 `STATUS_DLL_INIT_FAILED`（`-1073741502`），而显式 `--sandbox danger-full-access --always-approve` 可正常生成文件与运行 `node --test`。
- **规则**：自动化 Loop 在 Windows 下调用 `codex exec` 时默认采用 `--sandbox danger-full-access`（配合独占 `.worktrees/issue-<N>` 隔离），在文档和后续 Issue 中明确追踪 `agent-bridge` 的 Windows 沙箱默认策略。

### 3. Codex `0.172.0` TUI `PasteBurst` 吞掉括号粘贴后紧跟的 `\r`
- **现象**：`create --prompt` 作为命令行参数可直接触发首轮任务，但多轮调用 `send --text` 时将 `\x1b[200~...\x1b[201~\r` 一次性写入 ConPTY，Codex `0.172.0` 终端组件把瞬时 `\r` 判入粘贴突发窗口而未触发提交（文本停留在输入框）。
- **规则**：面向存活 TUI 会话的高层 `send --text` 若目标是 Codex TUI，需将粘贴体与提交回车做语义/时序隔离；在自动化无交互场景（如 `magpie-loop`）优先使用 `codex exec` stdin 模式。该问题已记录为 `v0.9.1` 首批待修 Issue。

### 4. Windows ConPTY 快速失败退出必须释放 `master` 句柄（`3d101ec`）
- **现象**：子进程启动后秒退（`exit_code = 1`），`mark_exit` 设置了 `process_done = true`，但 ConPTY 读端管道未收到 EOF（`reader_done = false`），导致会话长期停在 `phase = running`、`wait --for tui-idle` 超时 180 秒。
- **规则**：Windows 下 `mark_exit` 若尚未进入终态，必须关闭 writer 并释放 PTY `master`（触发 `ClosePseudoConsole` 让 reader 排空缓冲并收到 EOF），同时 `wait` 检查到 `process_done` 即可终止等待。

### 5. 本地工具链与 CI 工具链跨版本 `clippy` / `#[cfg]` 漂移（`e4d27ae`、`3d101ec`）
- **现象**：本地 Rust 1.95 零告警通过，但 GitHub Actions Ubuntu/Windows Runner 上的 Rust 1.100 对 `AtomicUsize::fetch_update`、`chunks_exact(2)`、`needless_bool` 及仅 Unix/Windows 下才未使用的 import 报错（`-D warnings`）。
- **规则**：新增/调整跨平台代码时确保 `#[cfg(windows)]` / `#[cfg(unix)]` 覆盖全部只读分支与 import；原子更新优先使用稳定便携原语（如 `compare_exchange_weak`）或标准块切片 API（`as_chunks`）；推送后务必等 6 平台 CI 全绿再打 Release Tag。

### 6. CLI `codex` 版本间的审批参数兼容性
- **现象**：`codex-cli 0.144.1` 无 `--approve-for-me`，而 `0.144.1` 与 `0.172.0` 均支持 `-a never`（`--ask-for-approval never`）。
- **规则**：拼装 Worker CLI 参数优先使用多版本共同支持的稳定标志，并保持与实测本机版本互认。
