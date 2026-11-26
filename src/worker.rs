use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use portable_pty::CommandBuilder;

use crate::protocol::{CodexSandboxMode, HookEvent, HookEventKind, SessionPhase, WorkerKind};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HookEffect {
    Reset,
    Working {
        tool_name: Option<String>,
    },
    Waiting {
        tool_name: Option<String>,
        reason: String,
    },
    Done,
    RecordOnly,
}

const PARENT_HOST_ENV_KEYS: &[&str] = &[
    "ANTIGRAVITY_CONVERSATION_ID",
    "ANTIGRAVITY_TRAJECTORY_ID",
    "ANTIGRAVITY_AGENT",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "AGENT_BRIDGE_CLIENT_KIND",
    "AGENT_BRIDGE_CLIENT_SESSION_ID",
];

pub(crate) fn resolve_worker_bin(worker: WorkerKind) -> OsString {
    let (env_key, stem, fallback) = match worker {
        WorkerKind::Grok => ("GROK_BIN", "grok", default_grok_bin()),
        WorkerKind::Codex => ("CODEX_BIN", "codex", default_codex_bin()),
    };
    if let Some(value) = env::var_os(env_key).filter(|value| !value.is_empty()) {
        return value;
    }
    #[cfg(windows)]
    if let Some(found) = find_in_windows_path(stem) {
        return found;
    }
    #[cfg(not(windows))]
    let _ = stem;
    fallback
}

#[cfg(windows)]
fn find_in_windows_path(stem: &str) -> Option<OsString> {
    let path_env = env::var_os("PATH")?;
    for dir in env::split_paths(&path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in ["exe", "cmd", "bat"] {
            let candidate = dir.join(format!("{stem}.{ext}"));
            if candidate.is_file() {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

fn is_windows_cmd_script(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

fn strip_enclosing_quotes(token: &str) -> &str {
    token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(token)
}

fn expand_cmd_dir_token(token: &str, cmd_dir: &Path) -> Option<PathBuf> {
    let unquoted = strip_enclosing_quotes(token.trim());
    for prefix in ["%~dp0\\", "%~dp0/", "%~dp0", "%dp0%\\", "%dp0%/", "%dp0%"] {
        if let Some(rest) = unquoted.strip_prefix(prefix) {
            let mut path = cmd_dir.to_path_buf();
            for component in rest.split(['\\', '/']) {
                if !component.is_empty() {
                    path.push(component);
                }
            }
            return Some(path);
        }
    }
    None
}

pub(crate) fn parse_windows_node_cmd_shim(
    cmd_dir: &Path,
    content: &str,
) -> Option<(OsString, Vec<OsString>)> {
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if !line.contains("%*") || !line.to_ascii_lowercase().contains("node_modules") {
            continue;
        }
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        for ch in line.chars() {
            match ch {
                '"' => {
                    in_quotes = !in_quotes;
                    current.push(ch);
                }
                c if c.is_whitespace() && !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }

        for token in tokens {
            let unquoted = strip_enclosing_quotes(&token);
            let lower = unquoted.to_ascii_lowercase();
            if !(lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")) {
                continue;
            }
            let script_path =
                expand_cmd_dir_token(unquoted, cmd_dir).unwrap_or_else(|| cmd_dir.join(unquoted));
            let local_node = cmd_dir.join("node.exe");
            let node_bin = if local_node.is_file() {
                local_node.into_os_string()
            } else {
                OsString::from("node.exe")
            };
            return Some((node_bin, vec![script_path.into_os_string()]));
        }
    }
    None
}

fn resolve_launch_target(bin: &OsStr) -> (OsString, Vec<OsString>) {
    let path = Path::new(bin);
    if !is_windows_cmd_script(path) {
        return (bin.to_os_string(), Vec::new());
    }
    if let (Some(cmd_dir), Ok(content)) = (path.parent(), fs::read_to_string(path))
        && let Some(resolved) = parse_windows_node_cmd_shim(cmd_dir, &content)
    {
        return resolved;
    }
    (
        OsString::from("cmd.exe"),
        vec![
            OsString::from("/d"),
            OsString::from("/c"),
            bin.to_os_string(),
        ],
    )
}

pub(crate) fn build_probe_command(bin: &OsStr) -> std::process::Command {
    let (program, prefix_args) = resolve_launch_target(bin);
    let mut command = std::process::Command::new(program);
    command.args(prefix_args);
    command
}

pub(crate) fn default_grok_bin() -> OsString {
    if cfg!(windows) {
        OsString::from("grok.exe")
    } else {
        OsString::from("grok")
    }
}

pub(crate) fn default_codex_bin() -> OsString {
    if cfg!(windows) {
        OsString::from("codex.exe")
    } else {
        OsString::from("codex")
    }
}

pub(crate) fn strip_parent_host_env(command: &mut CommandBuilder) {
    for key in PARENT_HOST_ENV_KEYS {
        command.env_remove(key);
    }
}

#[cfg(windows)]
pub(crate) fn strip_parent_host_env_std(command: &mut std::process::Command) {
    for key in PARENT_HOST_ENV_KEYS {
        command.env_remove(key);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_worker_command(
    worker: WorkerKind,
    bin: &OsString,
    cwd: &Path,
    prompt: Option<&str>,
    model: Option<&str>,
    always_approve: bool,
    sandbox: Option<CodexSandboxMode>,
    dangerously_bypass: bool,
    provider_session_id: &str,
    state_dir: Option<&Path>,
) -> CommandBuilder {
    let (program, prefix_args) = resolve_launch_target(bin.as_os_str());
    let mut command = CommandBuilder::new(program);
    for arg in prefix_args {
        command.arg(arg);
    }
    command.cwd(cwd.as_os_str());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    strip_parent_host_env(&mut command);

    match worker {
        WorkerKind::Grok => {
            if let Some(state_dir) = state_dir {
                command.env("GROK_HOME", state_dir.as_os_str());
            }
            command.arg("--session-id");
            command.arg(provider_session_id);
            if always_approve {
                command.arg("--always-approve");
            }
            if let Some(model) = model {
                command.arg("--model");
                command.arg(model);
            }
            if let Some(prompt) = prompt {
                command.arg(prompt);
            }
        }
        WorkerKind::Codex => {
            command.env("AGENT_BRIDGE_SESSION_ID", provider_session_id);
            command.env("AGENT_BRIDGE_WORKER", WorkerKind::Codex.as_str());
            command.arg("-C");
            command.arg(cwd.as_os_str());
            if let Some(model) = model {
                command.arg("-m");
                command.arg(model);
            }
            if dangerously_bypass {
                command.arg("--dangerously-bypass-approvals-and-sandbox");
            } else {
                let effective_sandbox =
                    sandbox.or_else(|| always_approve.then_some(CodexSandboxMode::WorkspaceWrite));
                if let Some(mode) = effective_sandbox {
                    command.arg("-s");
                    command.arg(mode.as_cli_arg());
                }
                if always_approve {
                    command.arg("-a");
                    command.arg("never");
                }
            }
            if let Some(prompt) = prompt {
                command.arg(prompt);
            }
        }
    }

    command
}

pub(crate) fn ensure_worker_state_dir_writable(
    worker: WorkerKind,
    cwd: &Path,
    provider_session_id: &str,
) -> Result<Option<PathBuf>> {
    let Some(state_dir) = worker_state_dir(worker, cwd) else {
        return Ok(None);
    };
    ensure_worker_state_dir_writable_at(worker, &state_dir, provider_session_id)?;
    Ok(Some(state_dir))
}

pub(crate) fn worker_state_dir(worker: WorkerKind, cwd: &Path) -> Option<PathBuf> {
    let (env_name, default_dir) = match worker {
        WorkerKind::Grok => ("GROK_HOME", ".grok"),
        WorkerKind::Codex => ("CODEX_HOME", ".codex"),
    };
    let state_dir = worker_state_dir_from(
        env::var_os(env_name),
        env::var_os("HOME"),
        env::var_os("USERPROFILE"),
        cfg!(windows),
        default_dir,
    )?;
    Some(resolve_state_dir_from_cwd(cwd, state_dir))
}

fn resolve_state_dir_from_cwd(cwd: &Path, state_dir: PathBuf) -> PathBuf {
    if state_dir.is_absolute() {
        state_dir
    } else {
        cwd.join(state_dir)
    }
}

pub(crate) fn worker_state_dir_from(
    worker_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
    windows: bool,
    default_dir_name: &str,
) -> Option<PathBuf> {
    non_empty_path(worker_home).or_else(|| {
        let home = if windows {
            non_empty_path(user_profile).or_else(|| non_empty_path(home))
        } else {
            non_empty_path(home).or_else(|| non_empty_path(user_profile))
        }?;
        Some(home.join(default_dir_name))
    })
}

fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub(crate) fn ensure_worker_state_dir_writable_at(
    worker: WorkerKind,
    state_dir: &Path,
    provider_session_id: &str,
) -> Result<()> {
    let label = match worker {
        WorkerKind::Grok => "Grok",
        WorkerKind::Codex => "Codex",
    };
    let context = format!(
        "{label} state directory is not writable: {}. The Runtime may have inherited a filesystem sandbox; start agent-bridge server outside that sandbox and retry",
        state_dir.display()
    );
    fs::create_dir_all(state_dir).with_context(|| context.clone())?;
    let probe_path = state_dir.join(format!(".agent-bridge-write-probe-{provider_session_id}"));
    let mut created = false;
    let probe_result = (|| -> std::io::Result<()> {
        let mut probe = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)?;
        created = true;
        probe.write_all(b"agent-bridge")?;
        probe.flush()
    })();
    let cleanup_result = if created {
        match fs::remove_file(&probe_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    probe_result.with_context(|| context.clone())?;
    cleanup_result.with_context(|| {
        format!(
            "failed to remove {label} state probe: {}",
            probe_path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn blocked_reason(worker: WorkerKind, screen: &str) -> Option<&'static str> {
    match worker {
        WorkerKind::Grok => {
            if screen.contains("Run Grok Build in a project directory?") {
                Some("grok-project-directory")
            } else if screen.contains("Type your answer here") || screen.contains("Enter:submit") {
                Some("grok-interactive-prompt")
            } else {
                None
            }
        }
        WorkerKind::Codex => {
            if screen.contains("Trust this directory")
                || screen.contains("Do you trust the contents of this directory?")
                || screen.contains("trust the files in this folder")
                || screen.contains("Trust this folder?")
                || screen.contains("Trust and continue")
            {
                Some("codex-trust-directory")
            } else if screen.contains("trust these hooks")
                || screen.contains("untrusted hook")
                || screen.contains("Allow this hook")
            {
                Some("codex-hook-trust")
            } else if screen.contains("Yes, proceed")
                || screen.contains("Allow command")
                || screen.contains("Approve command")
                || screen.contains("Press enter to confirm")
            {
                Some("codex-approval-prompt")
            } else {
                None
            }
        }
    }
}

pub(crate) fn phase_from_title(worker: WorkerKind, title: Option<&str>) -> Option<SessionPhase> {
    let title = title?.trim();
    let lower = title.to_ascii_lowercase();
    let suffix = match worker {
        WorkerKind::Grok => "grok",
        WorkerKind::Codex => "codex",
    };
    let hyphen_suffix = format!(" - {suffix}");
    if title_has_braille_spinner(title)
        && (lower.ends_with(suffix) || lower.contains(&hyphen_suffix))
    {
        return Some(SessionPhase::Running);
    }
    if lower == suffix || lower.ends_with(&hyphen_suffix) {
        return Some(SessionPhase::Idle);
    }
    None
}

fn title_has_braille_spinner(title: &str) -> bool {
    title
        .chars()
        .next()
        .is_some_and(|character| ('\u{2800}'..='\u{28ff}').contains(&character))
}

pub(crate) fn hook_effect(event: &HookEvent) -> HookEffect {
    match event.kind {
        HookEventKind::SessionStart => HookEffect::Reset,
        HookEventKind::UserPromptSubmit => HookEffect::Working {
            tool_name: event.tool_name.clone(),
        },
        HookEventKind::PostToolUse
        | HookEventKind::PostToolUseFailure
        | HookEventKind::PermissionDenied
        | HookEventKind::PreCompact
        | HookEventKind::PostCompact => HookEffect::Working { tool_name: None },
        HookEventKind::PermissionRequest => HookEffect::Waiting {
            tool_name: event.tool_name.clone(),
            reason: event
                .message
                .clone()
                .or_else(|| event.tool_name.clone())
                .unwrap_or_else(|| "permission_request".to_owned()),
        },
        HookEventKind::PreToolUse if is_ask_user_question(event.tool_name.as_deref()) => {
            HookEffect::Waiting {
                tool_name: event.tool_name.clone(),
                reason: event
                    .message
                    .clone()
                    .unwrap_or_else(|| "ask_user_question".to_owned()),
            }
        }
        HookEventKind::PreToolUse => HookEffect::Working {
            tool_name: event.tool_name.clone(),
        },
        HookEventKind::Stop
        | HookEventKind::StopFailure
        | HookEventKind::SessionEnd
        | HookEventKind::Interrupt => HookEffect::Done,
        HookEventKind::Notification => notification_effect(event),
        HookEventKind::SubagentStart | HookEventKind::SubagentStop => HookEffect::RecordOnly,
    }
}

fn is_ask_user_question(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| name.eq_ignore_ascii_case("ask_user_question"))
}

fn notification_effect(event: &HookEvent) -> HookEffect {
    let notification_type = event
        .notification_type
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if notification_type == "permission_prompt" {
        return HookEffect::RecordOnly;
    }

    let level = event
        .level
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let message = event.message.as_deref().unwrap_or_default();
    let lower_message = message.to_ascii_lowercase();
    let waiting = [
        "permission",
        "question",
        "ask_user_question",
        "elicitation",
        "elicitation_dialog",
    ]
    .iter()
    .any(|value| notification_type == *value || level == *value)
        || ["permission", "approval", "approve", "question"]
            .iter()
            .any(|value| lower_message.contains(value))
        || ["权限", "授权", "批准", "问题", "确认"]
            .iter()
            .any(|value| message.contains(value));
    if waiting {
        let reason = event
            .message
            .clone()
            .or_else(|| event.notification_type.clone())
            .unwrap_or_else(|| "agent-hook-waiting".to_owned());
        return HookEffect::Waiting {
            tool_name: event.tool_name.clone(),
            reason,
        };
    }

    let done = [
        "idle_prompt",
        "input_prompt",
        "input_required",
        "user_input",
        "waiting_for_input",
    ]
    .iter()
    .any(|value| notification_type == *value || level == *value)
        || ["waiting for input", "waiting for your input"]
            .iter()
            .any(|value| lower_message.contains(value))
        || ["请输入", "等待输入", "需要输入"]
            .iter()
            .any(|value| message.contains(value));
    if done {
        HookEffect::Done
    } else {
        HookEffect::RecordOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
