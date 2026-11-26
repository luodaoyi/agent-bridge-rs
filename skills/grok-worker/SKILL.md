---
name: grok-worker
description: Use Grok Build CLI as a persistent local coding subagent via agent-bridge when Antigravity (AGY) owns roadmap planning, task contracts, independent verification, and adversarial code review/challenges.
---

# Grok Worker via Agent Bridge (AGY Supervisor)

Resolve `<skill-dir>` as the directory containing this `SKILL.md` (`skills/grok-worker/`), so the plugin root is `<plugin-root> = <skill-dir>/../..`. Resolve `<bridge>` once per task from the actual host OS and architecture:

- On Unix, run `uname -s` and `uname -m` once.
- On Windows, run `powershell -NoProfile -Command "[System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()"` once (`X64` -> `x86_64`, `Arm64` -> `arm64`).

| Host | Executable |
| --- | --- |
| Windows x86_64 | `<plugin-root>/bin/windows-x86_64/agent-bridge.exe` |
| Windows ARM64 | `<plugin-root>/bin/windows-arm64/agent-bridge.exe` |
| Linux x86_64 | `<plugin-root>/bin/linux-x86_64/agent-bridge` |
| Linux ARM64 | `<plugin-root>/bin/linux-arm64/agent-bridge` |
| macOS x86_64 | `<plugin-root>/bin/macos-x86_64/agent-bridge` |
| macOS ARM64 | `<plugin-root>/bin/macos-arm64/agent-bridge` |

## Workflow

1. **Bootstrap**:
   - `<bridge> hooks status --worker grok` (install with `hooks install --worker grok` if false).
   - `<bridge> server status` (start with `server start` if stopped).
2. **Plan & Dispatch**:
   - AGY writes a bounded delegation contract (objective, allowed write set, acceptance criteria, required checks, constraints) flattened to one line:
   ```text
   <bridge> create --worker grok --cwd <absolute-worktree-path> --owner "<short-AGY-conversation-title>" --prompt "<single-line-delegation-contract>" [--always-approve] [--model <model>]
   ```
3. **Wait & Independent Verification**:
   ```text
   <bridge> wait --session <session> --for tui-idle --timeout-ms 300000
   <bridge> read --session <session> --cursor 0 --limit 4096 --wait-ms 5000
   ```
   - Handle `blocked_reason` (`grok-project-directory`, `grok-interactive-prompt`, or hook waiting state) by inspecting `show` and replying with `send --text`.
   - Independently inspect `git status` / `git diff` and re-run required checks yourself.
4. **Challenge & Close**:
   - Send targeted follow-up fixes to the same session with `send --text` (max 3 challenge rounds before escalating to the user).
   - Close finished sessions with `<bridge> close --session <session>` or `<bridge> close-client`.
