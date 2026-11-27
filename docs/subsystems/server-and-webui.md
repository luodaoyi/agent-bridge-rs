# Server, WebUI, and Desktop Terminal

> 核心源码：[src/server.rs](../../src/server.rs)、[src/terminal_gui.rs](../../src/terminal_gui.rs)、[src/gui_fonts.rs](../../src/gui_fonts.rs)、[src/tui.rs](../../src/tui.rs)、[src/version_check.rs](../../src/version_check.rs)、[webui/](../../webui/index.html)

## 职责与边界

- [src/server.rs](../../src/server.rs)：维护本地 IPC 与 HTTP/WebSocket 服务器（默认仅绑定 loopback `127.0.0.1:47654`，可通过 `AGENT_BRIDGE_WEB_ADDR` 覆写），提供 `/api/sessions`、`/api/events`、`/api/version` 及按会话/按宿主分组关闭接口，内嵌 `webui/dist` 编译产物。
- [webui/](../../webui/index.html)：基于 Tabler + Bootstrap + xterm.js 的只读优先看板与可切换交互终端，展示 AGY/Codex 宿主徽章与 Codex/Grok 执行器徽章，内置 15 种语言目录及 RTL 支持。
- [src/terminal_gui.rs](../../src/terminal_gui.rs) & [src/gui_fonts.rs](../../src/gui_fonts.rs)：基于 `eframe`/`egui` 的原生接管终端（关闭窗口只 detach，不终止底层 Worker）；Windows 强制 `Consolas` 英文优先 + `微软雅黑` CJK fallback（支持 `AGENT_BRIDGE_CJK_FONT` 显式覆写）。
- [src/tui.rs](../../src/tui.rs)：无参数 TTY 启动时的中文配置与安装 TUI。

## 核心不变量

1. **严格仅限 Loopback 与 Origin/Header 防护**：WebUI 不设账号登录，绝不允许绑定非 loopback 地址；关闭与 WebSocket 升级接口强制校验同源及 `X-Agent-Bridge-WebUI` 头，连接数采用无锁可移植 CAS（`compare_exchange_weak`）有界准入。
2. **只读终端与租约续命**：WebUI 默认禁用键盘注入，开启后透传原始按键字节与 `resize`；仅显式交互式输入（`terminal_input`）刷新宿主租约，只读态连接与自动视口 `terminal_resize` 均不刷新租约。
3. **15 语言全量一致性**：修改 `webui/src/i18n/catalog/*.js` 须同步全部 15 个语种并通过 `i18n.test.jsx`；修改前端后必须重新运行 `npm run build` 更新 `webui/dist`。

## 验证命令

```sh
cd webui && npm test && npm run build
cargo test --locked server::
cargo test --locked terminal_gui::
cargo test --locked gui_fonts::
```
