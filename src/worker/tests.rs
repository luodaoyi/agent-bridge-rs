use super::command::PARENT_HOST_ENV_KEYS;
use super::*;
use std::{ffi::OsString, fs, path::Path};

use crate::protocol::{CodexSandboxMode, SessionPhase, WorkerKind};

#[test]
fn builds_codex_command_with_default_workspace_write_approval_and_strips_parent_env() {
    let cwd = Path::new(r"C:\repo\.worktrees\task\core");
    let command = build_worker_command(
        WorkerKind::Codex,
        &OsString::from("codex.exe"),
        cwd,
        Some("implement feature"),
        Some("gpt-5-codex"),
        true,
        None,
        false,
        "01234567-89ab-4def-8123-456789abcdef",
        None,
    );
    let argv = command
        .get_argv()
        .iter()
        .map(|item| item.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        argv,
        vec![
            "codex.exe",
            "-C",
            r"C:\repo\.worktrees\task\core",
            "-m",
            "gpt-5-codex",
            "-s",
            "workspace-write",
            "-a",
            "never",
            "implement feature",
        ]
    );
    assert_eq!(
        command
            .get_env("AGENT_BRIDGE_SESSION_ID")
            .map(|v| v.to_string_lossy().into_owned()),
        Some("01234567-89ab-4def-8123-456789abcdef".to_owned())
    );
    assert_eq!(
        command
            .get_env("AGENT_BRIDGE_WORKER")
            .map(|v| v.to_string_lossy().into_owned()),
        Some("codex".to_owned())
    );
    for stripped in PARENT_HOST_ENV_KEYS {
        assert_eq!(command.get_env(stripped), None);
    }
}

#[test]
fn builds_codex_command_with_dangerously_bypass() {
    let command = build_worker_command(
        WorkerKind::Codex,
        &OsString::from("codex"),
        Path::new("/tmp/repo"),
        None,
        None,
        true,
        Some(CodexSandboxMode::ReadOnly),
        true,
        "01234567-89ab-4def-8123-456789abcdef",
        None,
    );
    let argv = command
        .get_argv()
        .iter()
        .map(|item| item.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        argv,
        vec![
            "codex",
            "-C",
            "/tmp/repo",
            "--dangerously-bypass-approvals-and-sandbox"
        ]
    );
}

#[test]
fn detects_codex_and_grok_blockers_and_titles() {
    assert_eq!(
        blocked_reason(
            WorkerKind::Codex,
            "Do you trust the contents of this directory?"
        ),
        Some("codex-trust-directory")
    );
    assert_eq!(
        blocked_reason(WorkerKind::Codex, "Allow this hook?"),
        Some("codex-hook-trust")
    );
    assert_eq!(
        blocked_reason(WorkerKind::Codex, "1. Yes, proceed"),
        Some("codex-approval-prompt")
    );
    assert_eq!(
        blocked_reason(WorkerKind::Grok, "Run Grok Build in a project directory?"),
        Some("grok-project-directory")
    );
    assert_eq!(
        phase_from_title(WorkerKind::Codex, Some("⠋ - Thinking - codex")),
        Some(SessionPhase::Running)
    );
    assert_eq!(
        phase_from_title(WorkerKind::Codex, Some("codex")),
        Some(SessionPhase::Idle)
    );
}

#[test]
fn parses_pnpm_and_npm_windows_cmd_node_shims() {
    let dir = tempfile::tempdir().unwrap();
    let shim_content = r#"@SETLOCAL
@IF EXIST "%~dp0\node.exe" (
  "%~dp0\node.exe"  "%~dp0\..\global\v11\pkg\node_modules\@openai\codex\bin\codex.js" %*
) ELSE (
  @SET PATHEXT=%PATHEXT:;.JS;=;%
  node  "%~dp0\..\global\v11\pkg\node_modules\@openai\codex\bin\codex.js" %*
)
"#;
    let (node_bin, args) = parse_windows_node_cmd_shim(dir.path(), shim_content).unwrap();
    assert_eq!(node_bin, OsString::from("node.exe"));
    assert_eq!(
        args,
        vec![
            dir.path()
                .join("..")
                .join("global")
                .join("v11")
                .join("pkg")
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js")
                .into_os_string()
        ]
    );

    let local_node = dir.path().join("node.exe");
    fs::write(&local_node, b"").unwrap();
    let npm_shim = "\"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\n";
    let (local_node_bin, npm_args) = parse_windows_node_cmd_shim(dir.path(), npm_shim).unwrap();
    assert_eq!(local_node_bin, local_node.into_os_string());
    assert_eq!(
        npm_args,
        vec![
            dir.path()
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js")
                .into_os_string()
        ]
    );
}

#[test]
fn build_worker_command_unwraps_cmd_shim_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cmd_path = dir.path().join("codex.CMD");
    fs::write(
        &cmd_path,
        "node \"%~dp0\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\n",
    )
    .unwrap();

    let command = build_worker_command(
        WorkerKind::Codex,
        &cmd_path.clone().into_os_string(),
        dir.path(),
        Some("hello <world> & test"),
        None,
        true,
        None,
        false,
        "01234567-89ab-4def-8123-456789abcdef",
        None,
    );
    let argv = command
        .get_argv()
        .iter()
        .map(|item| item.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(argv[0], "node.exe");
    assert!(argv[1].ends_with("codex.js"), "argv[1]={}", argv[1]);
    assert_eq!(
        &argv[2..],
        &[
            "-C",
            &dir.path().to_string_lossy(),
            "-s",
            "workspace-write",
            "-a",
            "never",
            "hello <world> & test"
        ]
    );
}

#[test]
fn detects_codex_0172_folder_access_trust_prompt() {
    let screen = "\n  Folder access\n  D:\\sources\\repos-new\\gh\\agent-bridge-rs\\.worktrees\\refactor-session-split\n\n  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings.\n\n› 1. Trust and continue\n  2. Back to Agent Command Center\n\n  enter continue · esc back\n";
    assert_eq!(
        blocked_reason(WorkerKind::Codex, screen),
        Some("codex-trust-directory")
    );
}

#[test]
fn detects_codex_0172_hook_review_prompt_and_ready_or_spinner_titles() {
    let hook_screen = "\n  Hooks need review\n\n  1. Review hooks\n› 2. Trust all and continue\n";
    assert_eq!(
        blocked_reason(WorkerKind::Codex, hook_screen),
        Some("codex-hook-trust")
    );
    assert_eq!(
        phase_from_title(WorkerKind::Codex, Some("Ready | Modularize Rust transport")),
        Some(SessionPhase::Idle)
    );
    assert_eq!(
        phase_from_title(WorkerKind::Codex, Some("Ready")),
        Some(SessionPhase::Idle)
    );
    assert_eq!(
        phase_from_title(WorkerKind::Codex, Some("⠋ Thinking...")),
        Some(SessionPhase::Running)
    );
}
