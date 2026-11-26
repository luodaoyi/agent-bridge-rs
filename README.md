# agent-bridge Local Runtime (Codex & Antigravity)

[English](README.md) | [简体中文](README-CN.md)

`agent-bridge` is a local, single-runtime PTY bridge supporting a bounded **2×2 supervisor/worker matrix**:

- **Codex → Grok** (`codex` host delegating implementation tasks to `grok`)
- **AGY → Codex** (`Antigravity` planning/reviewing/challenging while `codex` executes inside a worktree)
- **AGY → Grok** (`Antigravity` planning/reviewing/challenging while `grok` executes)

It is completely isolated from legacy `grok-bridge` (`agent-bridge-runtime-v1-*` IPC, `127.0.0.1:47654` WebUI, `AGENT_BRIDGE_*` env vars).

## Quick Start

### 1. Prerequisites

Ensure the target worker CLIs (`codex` and/or `grok`) are installed and authenticated on your machine:

```bash
codex --version
grok --version
```

### 2. Install & Deploy Host Integrations

```bash
pnpm add -g agent-bridge-rs@latest
# or run from source: cargo build --release
agent-bridge install
```

`agent-bridge install` deploys:
- **Codex Skill**: `~/.agents/skills/agent-bridge/` (`SKILL.md`, `agents/openai.yaml`, binary)
- **Antigravity (AGY) Native Plugin**: `~/.gemini/config/plugins/agent-bridge/` (`plugin.json`, `skills/codex-worker/SKILL.md`, `skills/grok-worker/SKILL.md`, binary)
- **Lifecycle Hooks**:
  - Grok hooks → `~/.grok/hooks/agent-bridge.json` (if legacy `grok-bridge.json` exists, a reminder is printed so you can delete it manually)
  - Codex hooks → non-destructively merged into `~/.codex/hooks.json` (preserving any existing hooks such as `rtk hook codex`)

### 3. CLI Usage

`--worker grok|codex` is required for `create` and creation-mode `terminal`:

```bash
# AGY -> Codex (defaults --always-approve to: -s workspace-write -a never)
agent-bridge create --worker codex --cwd /path/to/worktree --owner "my-task" --prompt "Objective: ..." --always-approve

# AGY/Codex -> Grok
agent-bridge create --worker grok --cwd /path/to/repo --owner "my-task" --prompt "Objective: ..." --always-approve

# Wait, inspect, challenge/rework, and clean up
agent-bridge wait --session <id> --for tui-idle --timeout-ms 300000
agent-bridge read --session <id> --cursor 0 --limit 4096 --wait-ms 5000
agent-bridge send --session <id> --text "Challenge round 1: ..."
agent-bridge close --session <id>
agent-bridge close-client   # closes all sessions owned by current CODEX_THREAD_ID / ANTIGRAVITY_CONVERSATION_ID

# WebUI & interactive takeover terminal
agent-bridge server ui     # http://127.0.0.1:47654
agent-bridge terminal --session <id>
agent-bridge doctor --worker all
```

## Environment Variables

| Variable | Purpose |
| --- | --- |
| `CODEX_BIN` / `GROK_BIN` | Override worker executable path |
| `AGENT_BRIDGE_WEB_ADDR` | Loopback WebUI listen address (default `127.0.0.1:47654`) |
| `AGENT_BRIDGE_ALLOWED_ROOTS` | Path list restricting accepted `--cwd` roots |
| `AGENT_BRIDGE_CLIENT_LEASE_SECONDS` | Client heartbeat lease duration (default `120`) |
| `AGENT_BRIDGE_ORPHAN_GRACE_SECONDS` | Idle orphan cleanup grace period (default `600`) |
| `AGENT_BRIDGE_CJK_FONT` / `AGENT_BRIDGE_CJK_FONT_INDEX` | Override egui CJK fallback font |
| `AGENT_BRIDGE_DISABLE_UPDATE_CHECK` | Disable GitHub release update check |
