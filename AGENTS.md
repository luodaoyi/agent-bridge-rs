# Repository Guidelines

## 项目结构与定位

本仓库构建跨平台 `agent-bridge`（支持 **Codex → Grok**、**AGY → Codex**、**AGY → Grok** 有界 2×2 矩阵的本地 Runtime）。`src/main.rs` 提供 CLI；`transport.rs` 负责自动启动、双宿主（Codex / Antigravity）身份探测和本地 IPC；`server.rs` 分发 RPC 与 WebUI；`session.rs` 持有 Worker PTY、有界终端状态与 Windows Job Object / Unix 进程组治理；`worker.rs` 封装 Grok 与 Codex 命令构建、阻塞提示与标题状态启发式；`hooks.rs` 管理 Grok 独立 Hook 文件与 Codex `hooks.json` 无损合并；`protocol.rs` 定义 JSON 请求和响应；`terminal_gui.rs` 实现 egui 真实终端；`gui_fonts.rs` 发现并验证 CJK fallback 字体；`install.rs` 部署 Codex Skill（`SKILL.md`）与 AGY 原生 Plugin（`plugin.json` + `skills/{codex,grok}-worker/SKILL.md`）。

保持 Orca 风格本地 Runtime，不扩展为网络服务、数据库、Git/worktree 管理器或开放式多 provider 抽象。

## 开发与编码规则

- 保持每用户单例 Server；CLI 通过本地 Named Pipe / Unix Socket（`agent-bridge-runtime-v1-*`）调用 `create/list/show/read/send/write/resize/wait/close`，默认 WebUI 端口 `127.0.0.1:47654`。
- `create` 与创建态 `terminal` 必须显式传 `--worker grok|codex`；Codex `--always-approve` 默认映射 `-s workspace-write -a never`，无沙箱直通需显式 `--dangerously-bypass`。
- 所有 Worker 进程和 ConPTY 均由 Server 创建并持有，且启动前必须剥离父宿主环境变量避免嵌套串租约。egui terminal 只附着会话，关窗口只 detach，显式 `close` 才终止会话。
- `send --text` 保持括号粘贴并追加 Enter 的高层语义；`write --data-base64` 必须逐字节写入，禁止隐式转码、追加回车或修改控制序列。
- `resize` 必须同时更新 ConPTY 和服务端 vt100 屏幕。`show` 保持 `rows`、`cols`、`screen_ansi_base64`，供 GUI 恢复当前状态。
- Named Pipe 使用一请求一响应 NDJSON，单帧保持 1 MiB 上限；保留 raw write 大小、终端行列、cursor 和参数验证。
- 启动外部程序必须使用独立参数，禁止 shell 拼接。`cwd` 必须 canonicalize，并保留 `AGENT_BRIDGE_ALLOWED_ROOTS`。
- GUI reader 和 writer 不得阻塞 egui 线程；后台结果到达后调用 `request_repaint`。窗口销毁只停止本地 reader，不发送 Close。
- cell renderer 必须保留颜色、样式、宽字符、光标、selection、copy 和 scrollback；输入需覆盖 IME、粘贴、控制键、Alt、导航键和 resize。
- Windows 英文必须优先使用 Consolas，中文必须由验证过字形覆盖的微软雅黑 fallback 显示；保留 `AGENT_BRIDGE_CJK_FONT`、`AGENT_BRIDGE_CJK_FONT_INDEX` 的显式中文字体覆盖和跨平台候选发现。
- Rust 使用 Rustfmt 默认风格：函数 `snake_case`、类型 `PascalCase`、常量 `SCREAMING_SNAKE_CASE`。

## 构建与测试

默认测试不得调用真实 Grok/Codex、消耗额度或修改外部仓库。变更 WebUI 后须先执行 `cd webui && npm ci && npm test && npm run build` 生成内嵌产物，再运行：

```text
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
node test/launcher.test.js
```

不要自动 commit、push、创建 Tag 或发布 Release。
