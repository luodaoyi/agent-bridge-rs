# AGENTS.md

> 本仓库所有 AI Agent（Antigravity / Codex / Claude Code / Grok）的**唯一权威入口**。动手前先读完本页与 [LESSONS.md](LESSONS.md)。

## 项目定位与边界

`agent-bridge-rs` 是本地优先的交互式 Subagent Runtime（CLI `agent-bridge`，默认 WebUI `127.0.0.1:47654`，IPC `agent-bridge-runtime-v1-*`），服务有界 2×2 调度矩阵：

- **Codex → Grok**
- **Antigravity (AGY) → Codex**
- **Antigravity (AGY) → Grok**

保持 Magpie/Orca 风格轻量本地守护进程：**不扩展为网络服务、数据库、Git/Worktree 管理器或开放式多 Provider 抽象**。

## 文档与避坑地图

修改任何模块前，先阅读 [编码规范与提交纪律](docs/code-standards.md)、[最近踩坑教训](LESSONS.md) 与对应的子系统参考卡：

| 子系统 | 说明文档 | 对应核心代码 |
| --- | --- | --- |
| 总索引与文档契约 | [docs/subsystems/README.md](docs/subsystems/README.md) | 跨模块联动规则与 PR 语义说明 |
| 协议与宿主租约 | [protocol-and-transport.md](docs/subsystems/protocol-and-transport.md) | [src/protocol.rs](src/protocol.rs)、[src/transport.rs](src/transport.rs) |
| 执行器与 PTY 生命周期 | [worker-and-pty.md](docs/subsystems/worker-and-pty.md) | [src/worker.rs](src/worker.rs)、[src/session.rs](src/session.rs) |
| Hooks 与双宿主安装器 | [hooks-and-installer.md](docs/subsystems/hooks-and-installer.md) | [src/hooks.rs](src/hooks.rs)、[src/install.rs](src/install.rs)、[skills/](skills/) |
| Server、WebUI 与桌面终端 | [server-and-webui.md](docs/subsystems/server-and-webui.md) | [src/server.rs](src/server.rs)、[src/terminal_gui.rs](src/terminal_gui.rs)、[webui/](webui/) |
| Issue Loop 与发版流水线 | [loop-and-release.md](docs/subsystems/loop-and-release.md) | [scripts/](scripts/)、[.github/workflows/](.github/workflows/) |

## 必跑质量门禁（提交前全量执行）

改动 `webui/` 时先跑前端构建（内嵌静态资源供 `src/server.rs` `include_bytes!` 编译）：

```sh
cd webui && npm ci && npm test && npm run build
```

每次代码或文档提交前必跑全量检查（禁止只跑单个测试子集后宣称通过）：

```sh
cargo fmt --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --locked
node test/launcher.test.js
node scripts/check-governance.mjs
node --test test/governance.test.js
```

若启用了 `CARGO_TARGET_DIR`，先用对应 target 下的二进制运行 `node test/launcher.test.js`（可通过 `AGENT_BRIDGE_TEST_BINARY` 指定）。默认自动化测试不得调用真实 `grok`/`codex` CLI、消耗额度或修改用户真实目录（`~/.codex/`、`~/.grok/`、`~/.agents/`、`~/.gemini/`）。

## 轻量协作与安全红线

1. **轻量 3 角色边界**：
   - **Maintainer（人）**：打 `ready` 标签准入 Issue、PR 合并终审、发版拍板。
   - **Implementer（本地 Worker / `magpie-loop`）**：在独占 `.worktrees/issue-<N>` 内先写失败复现测试，再做最小修复，跑通全量门禁后提 PR（禁止直接推 `main`、禁止 force-push、禁止自审自合）。
   - **CI & Review（GitHub Actions + CodeRabbit + Maintainer）**：6 平台构建/Clippy/Launcher 验证 + 治理路径检查 + 人类合并。
2. **通读全量评论**：处理任何 Issue 前必须读到最后一条评论；若存在“先别做 / 暂缓 / on hold”等信号，立即停止并标记 `needs:human`。
3. **禁止越权改流程红线**：自动修复 Loop 严禁修改 `.github/workflows/**` 或泄露任何密钥/Token；触及流程与依赖重大调整时标记 `needs:human` 交人拍板。
4. **验证证据留痕**：Commit Body 必须写明受影响子系统、实跑验证命令与关键回归测试名；只报告实际观察到的输出，禁止编造证据。
