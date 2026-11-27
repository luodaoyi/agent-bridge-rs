use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

use portable_pty::CommandBuilder;

use crate::protocol::{CodexSandboxMode, WorkerKind};

pub(super) const PARENT_HOST_ENV_KEYS: &[&str] = &[
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
