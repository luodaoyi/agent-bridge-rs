---
name: codex-worker
description: Use OpenAI Codex CLI as a persistent local coding subagent via agent-bridge when Antigravity (AGY) owns roadmap planning, task contracts, independent verification, and adversarial code review/challenges.
---

# Codex Worker via Agent Bridge (AGY Supervisor)

Resolve `<skill-dir>` as the directory containing this `SKILL.md` (`skills/codex-worker/`), so the plugin root is `<plugin-root> = <skill-dir>/../..`. Resolve `<bridge>` once per task from the actual host OS and architecture:

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

## Role Separation: AGY Plans & Challenges, Codex Executes

1. **AGY owns**: codebase exploration, roadmap and implementation plan, bounded delegation contract, independent `git diff` review, running test/build/lint gates directly, and adversarial challenge rounds.
2. **Codex owns**: concrete code edits, refactoring, unit tests, and self-checks inside the declared directory/worktree scope.
3. **Do NOT** edit the same files concurrently with Codex, and **do NOT** accept Codex's summary at face value without verifying `git diff` and running checks yourself.

## Standard 4-Stage Closed Loop

### Stage 1: Bootstrap (Before Any Auto-Starting RPC)
1. Check Codex hooks without modifying unrelated entries (e.g. `rtk hook codex`):
   - `<bridge> hooks status --worker codex`
   - If `installed: false`, run `<bridge> hooks install --worker codex` once from a writable context, then re-verify `hooks status --worker codex`.
2. Check Runtime liveness:
   - `<bridge> server status`
   - If stopped, run `<bridge> server start`, then confirm `server status` succeeds. (If the host's process container/Job Object terminates detached processes started by `server start` between tool calls, run `<bridge> server run` as a persistent background daemon task instead.)
   - Run `<bridge> doctor --worker codex` only if Codex CLI availability or hook trust is uncertain.

### Stage 2: AGY Plans & Dispatches Bounded Contract
Before calling `create`, write out a crisp delegation contract and flatten `--prompt` to a single line (no literal CR/LF):
- **Objective**: one bounded goal.
- **Write scope**: exact worktree/repo directory and files allowed (`-C` is set from `--cwd`; respect repository worktree rules, e.g. `.worktrees/<task>/<repo>`).
- **Acceptance criteria & required checks**: exact test/lint/build commands Codex must run.
- **Constraints**: forbidden files, no git commit/push/publish unless authorized.

Launch the Codex worker (default safe auto-approval uses `-s workspace-write -a never`):

```text
<bridge> create --worker codex --cwd <absolute-worktree-path> --owner "<short-AGY-conversation-title>" --prompt "<single-line-delegation-contract>" --always-approve [--model <model>]
```

- Use `--sandbox read-only` when asking Codex for analysis/review only.
- Use `--sandbox danger-full-access` or `--dangerously-bypass` ONLY when explicitly required and authorized by the user (e.g. in an externally sandboxed environment).
- Windows isolated host desktops (`WinSta0\exebox-*`): `agent-bridge` automatically resets detached server and gated worker launches back to `WinSta0\Default` when accessible and injects `--no-daemon` when supported by `codex` so default `-s workspace-write -a never` avoids lingering `exebox-*` `app-server` daemons and works with Codex `[windows] sandbox = "mxc"`. If a non-interactive environment or an older Codex CLI without `--no-daemon` still hits `-1073741502` (`STATUS_DLL_INIT_FAILED`), stop and ask the user before switching sandbox modes.
- AGY automatically propagates `ANTIGRAVITY_CONVERSATION_ID` (fallback `ANTIGRAVITY_TRAJECTORY_ID`) as the host lease identity, and `agent-bridge` strips parent AGY/Codex env vars from the child Codex process so nested calls never cross-bind.

### Stage 3: Wait, Inspect, and Independently Verify

```text
<bridge> wait --session <session> --for tui-idle --timeout-ms 300000
<bridge> read --session <session> --cursor 0 --limit 4096 --wait-ms 5000
```

- Check `blocked_reason`:
  - `codex-trust-directory` / `codex-hook-trust` / `codex-approval-prompt`: inspect `<bridge> show --session <session>` and either reply via `send --text` if authorized or ask the user / open `<bridge> terminal --session <session>` for takeover.
- Once idle, AGY MUST independently run:
  1. `git -C <worktree> status` and `git -C <worktree> diff`
  2. The required build / test / lint commands directly.

### Stage 4: Adversarial Challenge & Targeted Rework (Max 3 Rounds)
Evaluate Codex's diff against:
1. Requirement fidelity & missed edge cases
2. Error handling, resource/process lifecycle, and boundary conditions
3. Contract/type/protocol compatibility & unintended side effects
4. Unverified claims or skipped tests

- If defects exist, send concrete, line-referenced challenge feedback back into the **same** Codex session:

```text
<bridge> send --session <session> --text "Challenge round 1: (1) <file:line defect & counterexample>; (2) <missing edge case/test>. Fix these without changing unrelated files, re-run <check>, and report exact output."
<bridge> wait --session <session> --for tui-idle --timeout-ms 300000
```

- **Circuit breaker**: cap challenge rework at **3 rounds**. If unresolved after 3 rounds, stop dispatching, summarize the diff and disagreement for the user, and ask for a decision.
- Close completed sessions (`<bridge> close --session <session>`) or clean up all sessions created by this AGY conversation (`<bridge> close-client`).
