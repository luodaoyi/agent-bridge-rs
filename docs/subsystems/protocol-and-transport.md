# Protocol and Transport

> 核心源码：[src/protocol.rs](../../src/protocol.rs)、[src/transport.rs](../../src/transport.rs)

## 职责与边界

- 定义 CLI 客户端与每用户单例 Daemon 之间的本地 NDJSON 请求/响应契约（单帧上限 1 MiB）。
- 管理跨平台本地 IPC 传输（Windows Named Pipe 与 Unix Domain Socket，运行时命名空间 `agent-bridge-runtime-v1-*`），按需自动拉起后台 Daemon。
- 解析并校验当前调用方的宿主身份与租约 ID，实现多并发宿主会话隔离。

## 关键类型与符号

- [src/protocol.rs](../../src/protocol.rs)：
  - `WorkerKind`（`Grok` / `Codex`）：要求 `Request::Create` 与创建态 `terminal` 显式声明。
  - `ClientKind`（`Codex` / `Agy` / `Unknown`）与 `CodexSandboxMode`（`ReadOnly` / `WorkspaceWrite` / `DangerFullAccess`）。
  - `Request::CloseClient`（保留 `close_codex` serde 别名向后兼容）。
  - `validate_create` 等参数边界校验（禁止为 Grok 传 `--sandbox` / `--dangerously-bypass`）。
- [src/transport.rs](../../src/transport.rs)：
  - `current_client_identity()` / `client_identity_from()`：按 `AGENT_BRIDGE_CLIENT_*` → `CODEX_THREAD_ID`/`CODEX_SESSION_ID` → `ANTIGRAVITY_CONVERSATION_ID`/`ANTIGRAVITY_TRAJECTORY_ID` 优先级推导身份，生成 `codex:<id>`、`agy:<id>` 或 `unknown:<id>` 租约键。
  - `ShortTempDir` 与 Windows `PIPE_NOWAIT` 分块读写重试逻辑。

## 核心不变量

1. **严格租约隔离**：`close-client` / `heartbeat` 仅作用于当前 `client_session_id` 命名的会话集合，不得误关其他并行 AGY 或 Codex 对话派生的子进程。
2. **禁止跨租约污染**：子 Worker 启动时必须在 [src/worker.rs](../../src/worker.rs) 剥离所有父宿主环境变量，防止子 Agent 反向读取父进程 `ANTIGRAVITY_CONVERSATION_ID` 或 `CODEX_THREAD_ID`。
3. **帧长与参数在握手/入队前校验**：非法会话句柄、超长帧或非法 `Write` 字节长度必须立即拒绝，不得穿透到 PTY 或宿主管理状态。

## 验证命令

```sh
cargo test --locked protocol::
cargo test --locked transport::
```
