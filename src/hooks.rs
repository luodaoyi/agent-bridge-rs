use std::{
    env,
    ffi::OsString,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value};

mod config;
pub(crate) mod http_fallback;
#[cfg(test)]
mod tests;

use config::{hook_command, install_at, status_at, status_at_candidates, uninstall_at};

use crate::{
    protocol::{HookEvent, HookEventKind, Request, ResponseEnvelope, ResponseResult, WorkerKind},
    transport,
};

const GROK_HOOK_FILE_NAME: &str = "agent-bridge.json";
const LEGACY_GROK_HOOK_FILE_NAME: &str = "grok-bridge.json";
const CODEX_HOOK_FILE_NAME: &str = "hooks.json";
const MANAGED_ENV_NAME: &str = "AGENT_BRIDGE_MANAGED";
const MANAGED_ENV_VALUE: &str = "1";
const PROTOCOL_ENV_NAME: &str = "AGENT_BRIDGE_HOOK_PROTOCOL";
const PROTOCOL_ENV_VALUE: &str = "5";
const GROK_PROVIDER_SESSION_ENV_NAME: &str = "GROK_SESSION_ID";
const CODEX_PROVIDER_SESSION_ENV_NAME: &str = "AGENT_BRIDGE_SESSION_ID";
const MAX_STDIN_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_SESSION_ID_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 4096;
const MAX_SHORT_TEXT_BYTES: usize = 128;
const MAX_MESSAGE_BYTES: usize = 1024;
const HOOK_TIMEOUT_SECONDS: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HooksTarget {
    All,
    Grok,
    Codex,
}

impl std::str::FromStr for HooksTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "grok" => Ok(Self::Grok),
            "codex" => Ok(Self::Codex),
            other => bail!(
                "unsupported hooks worker target: {other}; expected 'all', 'grok', or 'codex'"
            ),
        }
    }
}

const GROK_HOOK_EVENTS: [HookSpec; 14] = [
    HookSpec::lifecycle("SessionStart"),
    HookSpec::lifecycle("UserPromptSubmit"),
    HookSpec::lifecycle("Stop"),
    HookSpec::lifecycle("StopFailure"),
    HookSpec::lifecycle("SessionEnd"),
    HookSpec::tool("PreToolUse"),
    HookSpec::tool("PostToolUse"),
    HookSpec::tool("PostToolUseFailure"),
    HookSpec::tool("PermissionDenied"),
    HookSpec::lifecycle("Notification"),
    HookSpec::lifecycle("SubagentStart"),
    HookSpec::lifecycle("SubagentStop"),
    HookSpec::lifecycle("PreCompact"),
    HookSpec::lifecycle("PostCompact"),
];

const CODEX_HOOK_EVENTS: [HookSpec; 8] = [
    HookSpec::lifecycle("SessionStart"),
    HookSpec::lifecycle("UserPromptSubmit"),
    HookSpec::tool("PreToolUse"),
    HookSpec::tool("PostToolUse"),
    HookSpec::tool("PermissionRequest"),
    HookSpec::lifecycle("Stop"),
    HookSpec::lifecycle("SessionEnd"),
    HookSpec::lifecycle("Interrupt"),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct HookInstallStatus {
    pub(crate) worker: WorkerKind,
    pub(crate) path: String,
    pub(crate) installed: bool,
    pub(crate) changed: bool,
    pub(crate) events_found: usize,
    pub(crate) events_expected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_grok_bridge_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AllHooksInstallStatus {
    pub(crate) grok: HookInstallStatus,
    pub(crate) codex: HookInstallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_grok_bridge_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum HooksCommandStatus {
    Single(HookInstallStatus),
    All(AllHooksInstallStatus),
}

#[derive(Clone, Copy)]
struct HookSpec {
    name: &'static str,
    tool_event: bool,
}

impl HookSpec {
    const fn lifecycle(name: &'static str) -> Self {
        Self {
            name,
            tool_event: false,
        }
    }

    const fn tool(name: &'static str) -> Self {
        Self {
            name,
            tool_event: true,
        }
    }
}

fn events_for(worker: WorkerKind) -> &'static [HookSpec] {
    match worker {
        WorkerKind::Grok => &GROK_HOOK_EVENTS,
        WorkerKind::Codex => &CODEX_HOOK_EVENTS,
    }
}

pub(crate) fn run_ingress_fail_open(worker: WorkerKind) {
    let Ok(input) = read_limited(io::stdin().lock()) else {
        return;
    };
    if input.overflowed {
        return;
    }

    let env_name = match worker {
        WorkerKind::Grok => GROK_PROVIDER_SESSION_ENV_NAME,
        WorkerKind::Codex => CODEX_PROVIDER_SESSION_ENV_NAME,
    };
    let provider_session_id = env::var(env_name).ok();
    let Some((provider_session_id, event)) =
        parse_event(&input.bytes, provider_session_id.as_deref())
    else {
        return;
    };

    let _ = deliver_hook_event_with(
        worker,
        provider_session_id,
        event,
        hook_endpoint_file_path_opt().as_deref(),
        |request| transport::call_anonymous(request, false),
    );
}

fn hook_endpoint_file_path_opt() -> Option<PathBuf> {
    http_fallback::hook_endpoint_file_path().ok()
}

fn deliver_hook_event_with<F>(
    worker: WorkerKind,
    provider_session_id: String,
    event: HookEvent,
    endpoint_file: Option<&Path>,
    ipc_call: F,
) -> bool
where
    F: FnOnce(Request) -> Result<ResponseEnvelope>,
{
    let request = Request::HookEvent {
        worker,
        provider_session_id: provider_session_id.clone(),
        event: event.clone(),
    };
    if let Ok(response) = ipc_call(request)
        && response.ok
        && matches!(response.result, Some(ResponseResult::Accepted { .. }))
    {
        return true;
    }
    let Some(endpoint_path) = endpoint_file else {
        return false;
    };
    http_fallback::send_via_loopback_http_from(endpoint_path, worker, &provider_session_id, &event)
        .is_ok()
}

pub(crate) fn install_target(target: HooksTarget) -> Result<HooksCommandStatus> {
    match target {
        HooksTarget::Grok => Ok(HooksCommandStatus::Single(install_for(WorkerKind::Grok)?)),
        HooksTarget::Codex => Ok(HooksCommandStatus::Single(install_for(WorkerKind::Codex)?)),
        HooksTarget::All => {
            let grok = install_for(WorkerKind::Grok)?;
            let codex = install_for(WorkerKind::Codex)?;
            let legacy_grok_bridge_path = grok.legacy_grok_bridge_path.clone();
            Ok(HooksCommandStatus::All(AllHooksInstallStatus {
                grok,
                codex,
                legacy_grok_bridge_path,
            }))
        }
    }
}

pub(crate) fn status_target(target: HooksTarget) -> Result<HooksCommandStatus> {
    match target {
        HooksTarget::Grok => Ok(HooksCommandStatus::Single(status_for(WorkerKind::Grok)?)),
        HooksTarget::Codex => Ok(HooksCommandStatus::Single(status_for(WorkerKind::Codex)?)),
        HooksTarget::All => {
            let grok = status_for(WorkerKind::Grok)?;
            let codex = status_for(WorkerKind::Codex)?;
            let legacy_grok_bridge_path = grok.legacy_grok_bridge_path.clone();
            Ok(HooksCommandStatus::All(AllHooksInstallStatus {
                grok,
                codex,
                legacy_grok_bridge_path,
            }))
        }
    }
}

pub(crate) fn uninstall_target(target: HooksTarget) -> Result<HooksCommandStatus> {
    match target {
        HooksTarget::Grok => Ok(HooksCommandStatus::Single(uninstall_for(WorkerKind::Grok)?)),
        HooksTarget::Codex => Ok(HooksCommandStatus::Single(uninstall_for(
            WorkerKind::Codex,
        )?)),
        HooksTarget::All => {
            let grok = uninstall_for(WorkerKind::Grok)?;
            let codex = uninstall_for(WorkerKind::Codex)?;
            let legacy_grok_bridge_path = grok.legacy_grok_bridge_path.clone();
            Ok(HooksCommandStatus::All(AllHooksInstallStatus {
                grok,
                codex,
                legacy_grok_bridge_path,
            }))
        }
    }
}

pub(crate) fn install_for(worker: WorkerKind) -> Result<HookInstallStatus> {
    let path = hook_file_path(worker)?;
    let command = current_hook_command(worker)?;
    let mut status = install_at(worker, &path, &command)?;
    if worker == WorkerKind::Grok {
        status.legacy_grok_bridge_path = legacy_grok_hook_path().map(|p| p.display().to_string());
    }
    Ok(status)
}

pub(crate) fn status_for(worker: WorkerKind) -> Result<HookInstallStatus> {
    let path = hook_file_path(worker)?;
    let mut candidates = vec![current_hook_command(worker)?];
    if let Ok(paths) = crate::install::Paths::discover() {
        for binary in [paths.agy_binary, paths.codex_binary] {
            if binary.is_file()
                && let Ok(command) = hook_command(worker, &binary)
                && !candidates.contains(&command)
            {
                candidates.push(command);
            }
        }
    }
    let mut status = status_at_candidates(worker, &path, &candidates)?;
    if worker == WorkerKind::Grok {
        status.legacy_grok_bridge_path = legacy_grok_hook_path().map(|p| p.display().to_string());
    }
    Ok(status)
}

pub(crate) fn uninstall_for(worker: WorkerKind) -> Result<HookInstallStatus> {
    let path = hook_file_path(worker)?;
    let command = current_hook_command(worker)?;
    let mut status = uninstall_at(worker, &path, &command)?;
    if worker == WorkerKind::Grok {
        status.legacy_grok_bridge_path = legacy_grok_hook_path().map(|p| p.display().to_string());
    }
    Ok(status)
}

pub fn install_at_path(
    worker: WorkerKind,
    binary: &Path,
    hook_path: &Path,
) -> Result<HookInstallStatus> {
    let command = hook_command(worker, binary)?;
    install_at(worker, hook_path, &command)
}

pub fn status_at_path(
    worker: WorkerKind,
    binary: &Path,
    hook_path: &Path,
) -> Result<HookInstallStatus> {
    let command = hook_command(worker, binary)?;
    status_at(worker, hook_path, &command)
}

pub fn uninstall_at_path(
    worker: WorkerKind,
    binary: &Path,
    hook_path: &Path,
) -> Result<HookInstallStatus> {
    let command = hook_command(worker, binary)?;
    uninstall_at(worker, hook_path, &command)
}

pub(crate) fn legacy_grok_hook_path() -> Option<PathBuf> {
    legacy_grok_hook_path_from(
        env::var_os("GROK_HOME"),
        env::var_os("HOME"),
        env::var_os("USERPROFILE"),
    )
    .filter(|path| path.exists())
}

fn legacy_grok_hook_path_from(
    grok_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    let grok_home = non_empty(grok_home);
    let home = non_empty(home);
    let user_profile = non_empty(user_profile);
    let root = if let Some(grok_home) = grok_home {
        PathBuf::from(grok_home)
    } else {
        #[cfg(windows)]
        let home = user_profile.or(home)?;
        #[cfg(not(windows))]
        let home = home.or(user_profile)?;
        PathBuf::from(home).join(".grok")
    };
    Some(root.join("hooks").join(LEGACY_GROK_HOOK_FILE_NAME))
}

struct LimitedInput {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn read_limited(mut reader: impl Read) -> io::Result<LimitedInput> {
    let mut bytes = Vec::with_capacity(8192);
    let mut buffer = [0_u8; 8192];
    let mut overflowed = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_STDIN_BYTES.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        overflowed |= retained != read;
    }
    Ok(LimitedInput { bytes, overflowed })
}

fn parse_event(
    input: &[u8],
    provider_session_from_env: Option<&str>,
) -> Option<(String, HookEvent)> {
    let value: Value = serde_json::from_slice(input).ok()?;
    let object = value.as_object()?;
    let event_name = string_field(object, "hookEventName", "hook_event_name")?;
    let kind = parse_event_kind(event_name)?;
    let provider_session_id = provider_session_from_env
        .and_then(|value| sanitize_text(value, MAX_PROVIDER_SESSION_ID_BYTES))
        .or_else(|| {
            string_field(object, "sessionId", "session_id")
                .and_then(|value| sanitize_text(value, MAX_PROVIDER_SESSION_ID_BYTES))
        })?
        .to_ascii_lowercase();

    Some((
        provider_session_id,
        HookEvent {
            kind,
            cwd: object
                .get("cwd")
                .and_then(Value::as_str)
                .and_then(|value| sanitize_text(value, MAX_CWD_BYTES)),
            tool_name: string_field(object, "toolName", "tool_name")
                .and_then(|value| sanitize_text(value, MAX_SHORT_TEXT_BYTES)),
            message: object
                .get("message")
                .and_then(Value::as_str)
                .and_then(|value| sanitize_text(value, MAX_MESSAGE_BYTES)),
            notification_type: string_field(object, "notificationType", "notification_type")
                .and_then(|value| sanitize_text(value, MAX_SHORT_TEXT_BYTES)),
            level: object
                .get("level")
                .and_then(Value::as_str)
                .and_then(|value| sanitize_text(value, MAX_SHORT_TEXT_BYTES)),
        },
    ))
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    camel_case: &str,
    snake_case: &str,
) -> Option<&'a str> {
    object
        .get(camel_case)
        .and_then(Value::as_str)
        .or_else(|| object.get(snake_case).and_then(Value::as_str))
}

fn parse_event_kind(value: &str) -> Option<HookEventKind> {
    match value {
        "SessionStart" | "session_start" => Some(HookEventKind::SessionStart),
        "UserPromptSubmit" | "user_prompt_submit" => Some(HookEventKind::UserPromptSubmit),
        "Stop" | "stop" => Some(HookEventKind::Stop),
        "StopFailure" | "stop_failure" => Some(HookEventKind::StopFailure),
        "SessionEnd" | "session_end" => Some(HookEventKind::SessionEnd),
        "PreToolUse" | "pre_tool_use" => Some(HookEventKind::PreToolUse),
        "PostToolUse" | "post_tool_use" => Some(HookEventKind::PostToolUse),
        "PostToolUseFailure" | "post_tool_use_failure" => Some(HookEventKind::PostToolUseFailure),
        "PermissionDenied" | "permission_denied" => Some(HookEventKind::PermissionDenied),
        "PermissionRequest" | "permission_request" => Some(HookEventKind::PermissionRequest),
        "Interrupt" | "interrupt" => Some(HookEventKind::Interrupt),
        "Notification" | "notification" => Some(HookEventKind::Notification),
        "SubagentStart" | "subagent_start" => Some(HookEventKind::SubagentStart),
        "SubagentStop" | "subagent_stop" => Some(HookEventKind::SubagentStop),
        "PreCompact" | "pre_compact" => Some(HookEventKind::PreCompact),
        "PostCompact" | "post_compact" => Some(HookEventKind::PostCompact),
        _ => None,
    }
}

fn sanitize_text(value: &str, max_bytes: usize) -> Option<String> {
    let mut sanitized = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars().filter(|character| !character.is_control()) {
        if sanitized.len() + character.len_utf8() > max_bytes {
            break;
        }
        sanitized.push(character);
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

pub(crate) fn hook_file_path(worker: WorkerKind) -> Result<PathBuf> {
    match worker {
        WorkerKind::Grok => grok_hook_file_path_from(
            env::var_os("GROK_HOME"),
            env::var_os("HOME"),
            env::var_os("USERPROFILE"),
        ),
        WorkerKind::Codex => codex_hook_file_path_from(
            env::var_os("CODEX_HOME"),
            env::var_os("HOME"),
            env::var_os("USERPROFILE"),
        ),
    }
}

fn grok_hook_file_path_from(
    grok_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Result<PathBuf> {
    let grok_home = non_empty(grok_home);
    let home = non_empty(home);
    let user_profile = non_empty(user_profile);
    let root = if let Some(grok_home) = grok_home {
        PathBuf::from(grok_home)
    } else {
        #[cfg(windows)]
        let home = user_profile.or(home);
        #[cfg(not(windows))]
        let home = home.or(user_profile);
        PathBuf::from(home.context("GROK_HOME, HOME, and USERPROFILE are not set")?).join(".grok")
    };
    Ok(root.join("hooks").join(GROK_HOOK_FILE_NAME))
}

fn codex_hook_file_path_from(
    codex_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Result<PathBuf> {
    let codex_home = non_empty(codex_home);
    let home = non_empty(home);
    let user_profile = non_empty(user_profile);
    let root = if let Some(codex_home) = codex_home {
        PathBuf::from(codex_home)
    } else {
        #[cfg(windows)]
        let home = user_profile.or(home);
        #[cfg(not(windows))]
        let home = home.or(user_profile);
        PathBuf::from(home.context("CODEX_HOME, HOME, and USERPROFILE are not set")?).join(".codex")
    };
    Ok(root.join(CODEX_HOOK_FILE_NAME))
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn current_hook_command(worker: WorkerKind) -> Result<String> {
    let executable = env::current_exe().context("failed to locate the current executable")?;
    if !executable.is_absolute() {
        bail!("current executable path is not absolute");
    }
    hook_command(worker, &executable)
}
