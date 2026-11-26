---
name: agent-bridge
description: Use Grok Build (or Codex) as a persistent local coding subagent for implementation, repair, testing, and follow-up work through the bundled agent-bridge Runtime CLI on Windows, Linux, or macOS. Use when the supervisor agent should delegate a bounded task while retaining planning, adversarial review, and final verification ownership, when machine-readable create/read/wait/send control or the localhost session WebUI is useful, or when the user explicitly asks for agent-bridge, grok, or codex subagents. Requires an authenticated worker CLI.
---

# Agent Bridge Local Runtime (Codex Host)

Resolve `<skill-dir>` as the directory containing this file. Resolve `<bridge>` once per task from the actual host OS and architecture; reuse that absolute path for every later command. Do not enumerate, sort, probe, or execute candidate binaries.

- On Unix, run `uname -s` and `uname -m` once.
- On Windows, run `powershell -NoProfile -Command "[System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()"` once. Map `X64` to x86_64 and `Arm64` to ARM64. If that API is unavailable, use `PROCESSOR_ARCHITEW6432` when set, otherwise `PROCESSOR_ARCHITECTURE`.
- Map only the detected pair through this table. Treat `uname -s` result `Darwin` as `macOS`, `AMD64` as x86_64, and `aarch64`/`arm64` as ARM64. If the pair is unsupported or the selected file is missing, stop and report it instead of trying another architecture.

| Host | Executable |
| --- | --- |
| Windows x86_64 | `<skill-dir>/bin/windows-x86_64/agent-bridge.exe` |
| Windows ARM64 | `<skill-dir>/bin/windows-arm64/agent-bridge.exe` |
| Linux x86_64 | `<skill-dir>/bin/linux-x86_64/agent-bridge` |
| Linux ARM64 | `<skill-dir>/bin/linux-arm64/agent-bridge` |
| macOS x86_64 | `<skill-dir>/bin/macos-x86_64/agent-bridge` |
| macOS ARM64 | `<skill-dir>/bin/macos-arm64/agent-bridge` |

Refer to the selected executable as `<bridge>` below. Do not download another wrapper, invoke Python, or fall back to a different bundled architecture.

## Subagent Model

Treat each created worker session as a persistent subagent handle, not as a one-shot shell command. The host agent (Codex) owns task decomposition, integration, user communication, independent diff/test verification, and challenge feedback; the worker (`--worker grok`) owns the concrete implementation task delegated to that session.

Before `create`, define a delegation contract containing:

- one bounded objective and the repository context needed to act;
- the allowed write set and any shared-resource lock;
- observable acceptance criteria and the exact checks to run;
- relevant repository constraints and prohibited external or irreversible actions.

Reuse the same session for questions, evidence, corrections, and retries on that task. For a user-authorized task in a trusted repository, pass `--always-approve` when the delegation contract is sufficiently bounded.

## Workflow

1. Inspect the repository, current changes, constraints, and acceptance criteria.
2. Bootstrap Hooks and the Runtime before any command that can auto-start the singleton:
   - Run `<bridge> hooks status --worker grok` first. If its JSON reports `installed: true`, do not run `hooks install`. Otherwise run `<bridge> hooks install --worker grok` from a writable user context, then confirm with `hooks status --worker grok`.
   - Run `<bridge> server status` before `list`, `create`, `show`, `read`, `send`, `write`, `resize`, `wait`, `close`, `heartbeat`, `close-client`, `terminal`, or `server ui`.
   - If `server status` reports stopped, run `<bridge> server start` from a writable user context and require a successful `server status` before continuing.
   - Run `<bridge> doctor --worker grok` only when worker availability is uncertain.
3. Create one focused session per delegated task. **`--worker` is required** (`--worker grok` when delegating from Codex). Flatten `--prompt` to one line (replace CR/LF with spaces).

```text
<bridge> create --worker grok --cwd <absolute-repository-path> --owner "<short-conversation-title>" --prompt "<single-line-delegation-contract>" [--always-approve] [--model <model>]
```

4. Wait for idle and read output:

```text
<bridge> wait --session <session> --for tui-idle --timeout-ms 300000
<bridge> read --session <session> --cursor 0 --limit 4096 --wait-ms 5000
```

5. Check `blocked_reason`, `activity`, `hook_event`, `tool_name`, and `waiting_reason`. If blocked, inspect `show` and reply with `send --text`. Refresh long leases with `heartbeat`.
6. When idle, independently inspect `git status`, `git diff`, and run required checks yourself — never trust worker self-reports alone. Send targeted follow-up fixes via `send --text` in the same session.
7. Close finished sessions with `close --session <session>` or end all sessions owned by this host conversation with `close-client` (alias `close-codex`).

## WebUI & Human Takeover

- `<bridge> server ui` opens the local WebUI (default `http://127.0.0.1:47654`, configured via `AGENT_BRIDGE_WEB_ADDR`).
- `<bridge> terminal --session <session>` attaches the egui terminal for manual takeover; closing the window only detaches.

## Command Rules

- Required `--worker grok|codex` on `create` and creation-mode `terminal`.
- `AGENT_BRIDGE_ALLOWED_ROOTS`, `AGENT_BRIDGE_CLIENT_LEASE_SECONDS`, `AGENT_BRIDGE_ORPHAN_GRACE_SECONDS`, `AGENT_BRIDGE_WEB_ADDR` govern policy; `GROK_BIN` / `CODEX_BIN` override worker binaries.
