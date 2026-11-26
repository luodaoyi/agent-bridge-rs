use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
#[cfg(any(windows, test))]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    protocol::{HookEvent, HookEventKind, Request, WorkerKind},
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

    let _ = transport::call_anonymous(
        Request::HookEvent {
            worker,
            provider_session_id,
            event,
        },
        false,
    );
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
    let command = current_hook_command(worker)?;
    let mut status = status_at(worker, &path, &command)?;
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

#[cfg(windows)]
fn hook_command(worker: WorkerKind, executable: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("current executable path is not valid UTF-8")?;
    if executable.chars().any(char::is_control) {
        bail!("current executable path cannot be represented in a hook command");
    }
    let worker_arg = worker.as_str();
    let shell_path = executable.replace('\\', "/");
    if shell_path.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'~' | b'-')
    }) {
        return Ok(format!(
            "{shell_path} __hook --worker {worker_arg} --protocol {PROTOCOL_ENV_VALUE}"
        ));
    }

    let quoted = executable.replace('\'', "''");
    let script = format!(
        "if (Test-Path -LiteralPath '{quoted}' -PathType Leaf) {{ & '{quoted}' __hook --worker {worker_arg} --protocol {PROTOCOL_ENV_VALUE}; exit $LASTEXITCODE }}; [Console]::In.ReadToEnd() | Out-Null; exit 0"
    );
    let mut encoded = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    let system_root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let powershell = format!(
        "{}/System32/WindowsPowerShell/v1.0/powershell.exe",
        system_root.replace('\\', "/").trim_end_matches('/')
    );
    if !powershell.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'~' | b'-')
    }) {
        bail!("Windows PowerShell path cannot be represented in a hook command");
    }
    Ok(format!(
        "{powershell} -NoProfile -ExecutionPolicy Bypass -EncodedCommand {}",
        BASE64.encode(encoded)
    ))
}

#[cfg(not(windows))]
fn hook_command(worker: WorkerKind, executable: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("current executable path is not valid UTF-8")?;
    if executable.chars().any(char::is_control) {
        bail!("current executable path cannot be represented in a hook command");
    }
    let quoted = executable.replace('\'', r#"'"'"'"#);
    Ok(format!(
        "'{quoted}' __hook --worker {} --protocol {PROTOCOL_ENV_VALUE}",
        worker.as_str()
    ))
}

fn install_at(worker: WorkerKind, path: &Path, command: &str) -> Result<HookInstallStatus> {
    let mut document = load_document(path)?.unwrap_or_else(|| json!({}));
    validate_document(&document)?;
    let current = inspect_document(worker, path, &document, command);
    if current.installed {
        return Ok(current);
    }

    let original = document.clone();
    remove_managed_hooks(worker, &mut document)?;
    add_managed_hooks(worker, &mut document, command)?;
    let changed = document != original;
    if changed {
        write_document(path, &document)?;
    }
    let mut result = inspect_document(worker, path, &document, command);
    result.changed = changed;
    Ok(result)
}

fn status_at(worker: WorkerKind, path: &Path, command: &str) -> Result<HookInstallStatus> {
    let Some(document) = load_document(path)? else {
        return Ok(empty_status(worker, path));
    };
    validate_document(&document)?;
    Ok(inspect_document(worker, path, &document, command))
}

fn uninstall_at(worker: WorkerKind, path: &Path, command: &str) -> Result<HookInstallStatus> {
    let Some(mut document) = load_document(path)? else {
        return Ok(empty_status(worker, path));
    };
    validate_document(&document)?;
    let changed = remove_managed_hooks(worker, &mut document)? != 0;
    if changed {
        write_document(path, &document)?;
    }
    let mut result = inspect_document(worker, path, &document, command);
    result.changed = changed;
    Ok(result)
}

fn empty_status(worker: WorkerKind, path: &Path) -> HookInstallStatus {
    HookInstallStatus {
        worker,
        path: path.display().to_string(),
        installed: false,
        changed: false,
        events_found: 0,
        events_expected: events_for(worker).len(),
        legacy_grok_bridge_path: None,
    }
}

fn load_document(path: &Path) -> Result<Option<Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let document = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(document))
}

fn validate_document(document: &Value) -> Result<()> {
    let object = document
        .as_object()
        .context("hook configuration root must be a JSON object")?;
    if object.get("hooks").is_some_and(|hooks| !hooks.is_object()) {
        bail!("hook configuration 'hooks' must be a JSON object");
    }
    Ok(())
}

fn write_document(path: &Path, document: &Value) -> Result<()> {
    let parent = path
        .parent()
        .context("hook configuration path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut bytes =
        serde_json::to_vec_pretty(document).context("failed to serialize hook configuration")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn inspect_document(
    worker: WorkerKind,
    path: &Path,
    document: &Value,
    command: &str,
) -> HookInstallStatus {
    let specs = events_for(worker);
    let hooks = document.get("hooks").and_then(Value::as_object);
    let managed_handlers = hooks.map_or(0, |hooks| count_managed_handlers(worker, hooks));
    let events_found = hooks.map_or(0, |hooks| {
        specs
            .iter()
            .filter(|spec| event_has_expected_hook(worker, hooks, **spec, command))
            .count()
    });
    HookInstallStatus {
        worker,
        path: path.display().to_string(),
        installed: events_found == specs.len() && managed_handlers == specs.len(),
        changed: false,
        events_found,
        events_expected: specs.len(),
        legacy_grok_bridge_path: None,
    }
}

fn count_managed_handlers(worker: WorkerKind, hooks: &Map<String, Value>) -> usize {
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|handler| is_managed_handler(worker, handler))
        .count()
}

fn event_has_expected_hook(
    worker: WorkerKind,
    hooks: &Map<String, Value>,
    spec: HookSpec,
    command: &str,
) -> bool {
    hooks
        .get(spec.name)
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                let Some(group) = group.as_object() else {
                    return false;
                };
                if spec.tool_event {
                    if group.get("matcher").and_then(Value::as_str) != Some(".*") {
                        return false;
                    }
                } else if group.contains_key("matcher") {
                    return false;
                }
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|handlers| {
                        handlers
                            .iter()
                            .any(|handler| is_expected_handler(worker, handler, command))
                    })
            })
        })
}

fn is_managed_handler(worker: WorkerKind, handler: &Value) -> bool {
    match worker {
        WorkerKind::Grok => {
            handler
                .get("env")
                .and_then(Value::as_object)
                .and_then(|env| env.get(MANAGED_ENV_NAME))
                .and_then(Value::as_str)
                == Some(MANAGED_ENV_VALUE)
                || command_matches_managed_worker(handler, WorkerKind::Grok)
        }
        WorkerKind::Codex => command_matches_managed_worker(handler, WorkerKind::Codex),
    }
}

fn command_matches_managed_worker(handler: &Value, worker: WorkerKind) -> bool {
    let Some(command) = handler.get("command").and_then(Value::as_str) else {
        return false;
    };
    let needle = format!("__hook --worker {}", worker.as_str());
    if command.contains(&needle) {
        return true;
    }
    #[cfg(any(windows, test))]
    if let Some(decoded) = decode_powershell_encoded_command(command)
        && decoded.contains(&needle)
    {
        return true;
    }
    false
}

#[cfg(any(windows, test))]
fn decode_powershell_encoded_command(command: &str) -> Option<String> {
    let marker = " -EncodedCommand ";
    let (_, encoded) = command.split_once(marker)?;
    let encoded_token = encoded.split_whitespace().next()?;
    let bytes = BASE64.decode(encoded_token).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let utf16 = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&utf16).ok()
}

fn is_expected_handler(worker: WorkerKind, handler: &Value, command: &str) -> bool {
    let base_ok = is_managed_handler(worker, handler)
        && handler.get("type").and_then(Value::as_str) == Some("command")
        && handler.get("command").and_then(Value::as_str) == Some(command)
        && handler.get("timeout").and_then(Value::as_u64) == Some(HOOK_TIMEOUT_SECONDS);
    if !base_ok {
        return false;
    }
    match worker {
        WorkerKind::Grok => {
            handler
                .get("env")
                .and_then(Value::as_object)
                .and_then(|env| env.get(PROTOCOL_ENV_NAME))
                .and_then(Value::as_str)
                == Some(PROTOCOL_ENV_VALUE)
                && has_expected_runtime_identity(handler)
        }
        WorkerKind::Codex => handler.get("env").is_none(),
    }
}

#[cfg(windows)]
fn has_expected_runtime_identity(handler: &Value) -> bool {
    let Some(handler_env) = handler.get("env").and_then(Value::as_object) else {
        return false;
    };
    let username = env::var("USERNAME").unwrap_or_else(|_| "default".to_owned());
    let domain = env::var("USERDOMAIN").unwrap_or_default();
    handler_env.get("USERNAME").and_then(Value::as_str) == Some(username.as_str())
        && handler_env.get("USERDOMAIN").and_then(Value::as_str) == Some(domain.as_str())
}

#[cfg(not(windows))]
fn has_expected_runtime_identity(_: &Value) -> bool {
    true
}

fn remove_managed_hooks(worker: WorkerKind, document: &mut Value) -> Result<usize> {
    let Some(hooks) = document.get_mut("hooks") else {
        return Ok(0);
    };
    let hooks = hooks
        .as_object_mut()
        .context("hook configuration 'hooks' must be a JSON object")?;
    let event_names = hooks.keys().cloned().collect::<Vec<_>>();
    let mut removed_total = 0;
    for event_name in event_names {
        let Some(groups) = hooks.get_mut(&event_name).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut removed_from_event = 0;
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|handler| !is_managed_handler(worker, handler));
            let removed = before - handlers.len();
            removed_from_event += removed;
            removed_total += removed;
            removed == 0 || !handlers.is_empty()
        });
        if removed_from_event != 0 && groups.is_empty() {
            hooks.remove(&event_name);
        }
    }
    Ok(removed_total)
}

fn add_managed_hooks(worker: WorkerKind, document: &mut Value, command: &str) -> Result<()> {
    let root = document
        .as_object_mut()
        .context("hook configuration root must be a JSON object")?;
    if !root.contains_key("hooks") {
        root.insert("hooks".to_owned(), json!({}));
    }
    let hooks = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .context("hook configuration 'hooks' must be a JSON object")?;
    for spec in events_for(worker) {
        let groups = hooks
            .entry(spec.name.to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("hook event '{}' must be a JSON array", spec.name))?;
        let handler = match worker {
            WorkerKind::Grok => {
                let mut handler_env = Map::new();
                handler_env.insert(
                    MANAGED_ENV_NAME.to_owned(),
                    Value::String(MANAGED_ENV_VALUE.to_owned()),
                );
                handler_env.insert(
                    PROTOCOL_ENV_NAME.to_owned(),
                    Value::String(PROTOCOL_ENV_VALUE.to_owned()),
                );
                #[cfg(windows)]
                {
                    handler_env.insert(
                        "USERNAME".to_owned(),
                        Value::String(
                            env::var("USERNAME").unwrap_or_else(|_| "default".to_owned()),
                        ),
                    );
                    handler_env.insert(
                        "USERDOMAIN".to_owned(),
                        Value::String(env::var("USERDOMAIN").unwrap_or_default()),
                    );
                }
                json!({
                    "type": "command",
                    "command": command,
                    "timeout": HOOK_TIMEOUT_SECONDS,
                    "env": handler_env,
                })
            }
            WorkerKind::Codex => {
                // Codex hooks.json rejects non-standard fields on command handlers;
                // identify managed entries strictly via the __hook --worker codex command.
                json!({
                    "type": "command",
                    "command": command,
                    "timeout": HOOK_TIMEOUT_SECONDS,
                })
            }
        };
        let group = if spec.tool_event {
            json!({
                "matcher": ".*",
                "hooks": [handler],
            })
        } else {
            json!({
                "hooks": [handler],
            })
        };
        groups.push(group);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("hook-tests")
                .join(format!(
                    "agent-bridge-hooks-test-{}-{timestamp}-{sequence}",
                    std::process::id()
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn grok_hook_file(&self) -> PathBuf {
            self.0.join("grok").join("hooks").join(GROK_HOOK_FILE_NAME)
        }

        fn codex_hook_file(&self) -> PathBuf {
            self.0.join("codex").join(CODEX_HOOK_FILE_NAME)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn drains_oversized_input_without_retaining_more_than_the_limit() {
        let data = vec![b'x'; MAX_STDIN_BYTES + 137];
        let mut cursor = Cursor::new(data);
        let input = read_limited(&mut cursor).unwrap();
        assert_eq!(input.bytes.len(), MAX_STDIN_BYTES);
        assert!(input.overflowed);
        assert_eq!(cursor.position(), (MAX_STDIN_BYTES + 137) as u64);
    }

    #[test]
    fn parses_all_supported_event_names() {
        let cases = [
            ("SessionStart", HookEventKind::SessionStart),
            ("UserPromptSubmit", HookEventKind::UserPromptSubmit),
            ("Stop", HookEventKind::Stop),
            ("StopFailure", HookEventKind::StopFailure),
            ("SessionEnd", HookEventKind::SessionEnd),
            ("PreToolUse", HookEventKind::PreToolUse),
            ("PostToolUse", HookEventKind::PostToolUse),
            ("PostToolUseFailure", HookEventKind::PostToolUseFailure),
            ("PermissionDenied", HookEventKind::PermissionDenied),
            ("PermissionRequest", HookEventKind::PermissionRequest),
            ("Interrupt", HookEventKind::Interrupt),
            ("Notification", HookEventKind::Notification),
            ("SubagentStart", HookEventKind::SubagentStart),
            ("SubagentStop", HookEventKind::SubagentStop),
            ("PreCompact", HookEventKind::PreCompact),
            ("PostCompact", HookEventKind::PostCompact),
        ];
        for (name, expected) in cases {
            let payload = serde_json::to_vec(&json!({
                "hookEventName": name,
                "sessionId": "019e37f4-5135-7b63-a4ab-6d13aa6bf528",
            }))
            .unwrap();
            assert_eq!(parse_event(&payload, None).unwrap().1.kind, expected);
        }
        assert!(parse_event(br#"{"hookEventName":"UnknownEvent"}"#, None).is_none());
        assert!(parse_event(br#"{"hookEventName":"Stop"}"#, None).is_none());
    }

    #[test]
    fn bundled_hook_templates_cover_the_current_protocol() {
        for source in [
            include_str!("../hooks/windows/agent-bridge-grok.json"),
            include_str!("../hooks/unix/agent-bridge-grok.json"),
        ] {
            let document: Value = serde_json::from_str(source).unwrap();
            let hooks = document["hooks"].as_object().unwrap();
            assert_eq!(hooks.len(), GROK_HOOK_EVENTS.len());
            for spec in GROK_HOOK_EVENTS {
                let groups = hooks[spec.name].as_array().unwrap();
                assert_eq!(groups.len(), 1);
                if spec.tool_event {
                    assert_eq!(groups[0]["matcher"], ".*");
                }
                let handler = &groups[0]["hooks"][0];
                assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECONDS);
                assert_eq!(handler["env"][MANAGED_ENV_NAME], MANAGED_ENV_VALUE);
                assert_eq!(handler["env"][PROTOCOL_ENV_NAME], PROTOCOL_ENV_VALUE);
            }
        }

        for source in [
            include_str!("../hooks/windows/agent-bridge-codex.json"),
            include_str!("../hooks/unix/agent-bridge-codex.json"),
        ] {
            let document: Value = serde_json::from_str(source).unwrap();
            let hooks = document["hooks"].as_object().unwrap();
            assert_eq!(hooks.len(), CODEX_HOOK_EVENTS.len());
            for spec in CODEX_HOOK_EVENTS {
                let groups = hooks[spec.name].as_array().unwrap();
                assert_eq!(groups.len(), 1);
                if spec.tool_event {
                    assert_eq!(groups[0]["matcher"], ".*");
                }
                let handler = &groups[0]["hooks"][0];
                assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECONDS);
                assert!(handler.get("env").is_none());
                assert!(
                    handler["command"]
                        .as_str()
                        .unwrap()
                        .contains("__hook --worker codex")
                );
            }
        }
    }

    #[test]
    fn accepts_snake_case_fields_and_prefers_provider_session_environment() {
        let payload = serde_json::to_vec(&json!({
            "hook_event_name": "notification",
            "session_id": "json-session",
            "cwd": "repo\nroot",
            "tool_name": "read\u{0}file",
            "message": "message\r\nbody",
            "notification_type": "permission\trequest",
            "level": "warn\n",
        }))
        .unwrap();
        let (provider_session_id, event) =
            parse_event(&payload, Some("019e37f4-5135-7b63-a4ab-6d13aa6bf528\n")).unwrap();
        assert_eq!(provider_session_id, "019e37f4-5135-7b63-a4ab-6d13aa6bf528");
        assert_eq!(event.cwd.as_deref(), Some("reporoot"));
        assert_eq!(event.tool_name.as_deref(), Some("readfile"));
        assert_eq!(event.message.as_deref(), Some("messagebody"));
        assert_eq!(
            event.notification_type.as_deref(),
            Some("permissionrequest")
        );
        assert_eq!(event.level.as_deref(), Some("warn"));
    }

    #[test]
    fn truncates_text_on_utf8_boundaries() {
        let sanitized = sanitize_text(&"中".repeat(100), MAX_SHORT_TEXT_BYTES).unwrap();
        assert!(sanitized.len() <= MAX_SHORT_TEXT_BYTES);
        assert!(sanitized.is_char_boundary(sanitized.len()));
        assert_eq!(sanitized.chars().count(), MAX_SHORT_TEXT_BYTES / 3);
    }

    #[test]
    fn resolves_grok_and_codex_hook_paths() {
        let grok_path = grok_hook_file_path_from(
            Some(OsString::from("custom-grok-home")),
            Some(OsString::from("home")),
            Some(OsString::from("profile")),
        )
        .unwrap();
        assert_eq!(
            grok_path,
            PathBuf::from("custom-grok-home")
                .join("hooks")
                .join(GROK_HOOK_FILE_NAME)
        );

        let codex_path = codex_hook_file_path_from(
            Some(OsString::from("custom-codex-home")),
            Some(OsString::from("home")),
            Some(OsString::from("profile")),
        )
        .unwrap();
        assert_eq!(
            codex_path,
            PathBuf::from("custom-codex-home").join(CODEX_HOOK_FILE_NAME)
        );
    }

    #[cfg(windows)]
    #[test]
    fn builds_fast_and_encoded_windows_hook_commands() {
        assert_eq!(
            hook_command(WorkerKind::Codex, Path::new(r"C:\tools\agent-bridge.exe")).unwrap(),
            "C:/tools/agent-bridge.exe __hook --worker codex --protocol 5"
        );

        let command = hook_command(
            WorkerKind::Codex,
            Path::new(r"C:\Program Files\Agent Bridge\agent-bridge.exe"),
        )
        .unwrap();
        assert!(command.contains(" -NoProfile -ExecutionPolicy Bypass -EncodedCommand "));
        assert!(!command.contains("Program Files"));
        let script = decode_powershell_encoded_command(&command).unwrap();
        assert!(script.contains(
            "& 'C:\\Program Files\\Agent Bridge\\agent-bridge.exe' __hook --worker codex --protocol 5"
        ));
        assert!(script.contains("[Console]::In.ReadToEnd() | Out-Null"));
    }

    #[test]
    fn grok_installation_preserves_foreign_hooks_and_is_idempotent() {
        let directory = TestDir::new();
        let path = directory.grok_hook_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = json!({
            "metadata": { "owner": "user" },
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "foreign-command",
                        "timeout": 5,
                    }],
                }],
            },
        });
        write_document(&path, &foreign).unwrap();

        let cmd = r#""C:\Program Files\agent-bridge.exe" __hook --worker grok --protocol 5"#;
        let first = install_at(WorkerKind::Grok, &path, cmd).unwrap();
        assert!(first.installed);
        assert!(first.changed);
        assert_eq!(first.events_found, GROK_HOOK_EVENTS.len());
        let first_bytes = fs::read(&path).unwrap();

        let second = install_at(WorkerKind::Grok, &path, cmd).unwrap();
        assert!(second.installed);
        assert!(!second.changed);
        assert_eq!(fs::read(&path).unwrap(), first_bytes);

        let document = load_document(&path).unwrap().unwrap();
        assert_eq!(document["metadata"]["owner"], "user");
        let pre_tool_groups = document["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre_tool_groups
                .iter()
                .any(|group| group["matcher"] == "Bash")
        );
    }

    #[test]
    fn codex_installation_merges_without_touching_rtk_hook_and_uninstall_preserves_it() {
        let directory = TestDir::new();
        let path = directory.codex_hook_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let existing = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook codex",
                    }],
                }],
            },
        });
        write_document(&path, &existing).unwrap();

        let cmd = r"C:/tools/agent-bridge.exe __hook --worker codex --protocol 5";
        let installed = install_at(WorkerKind::Codex, &path, cmd).unwrap();
        assert!(installed.installed);
        assert!(installed.changed);
        assert_eq!(installed.events_found, CODEX_HOOK_EVENTS.len());

        let doc = load_document(&path).unwrap().unwrap();
        let pre_tool = doc["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2);
        assert!(
            pre_tool.iter().any(|g| {
                g["matcher"] == "Bash" && g["hooks"][0]["command"] == "rtk hook codex"
            })
        );
        assert!(pre_tool.iter().any(|g| {
            g["matcher"] == ".*"
                && g["hooks"][0]["command"] == cmd
                && g["hooks"][0].get("env").is_none()
        }));

        // Re-running is idempotent.
        let again = install_at(WorkerKind::Codex, &path, cmd).unwrap();
        assert!(again.installed);
        assert!(!again.changed);

        // Uninstall removes only agent-bridge hooks and keeps rtk intact.
        let removed = uninstall_at(WorkerKind::Codex, &path, cmd).unwrap();
        assert!(!removed.installed);
        assert!(removed.changed);
        let after = load_document(&path).unwrap().unwrap();
        let pre_tool_after = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_after.len(), 1);
        assert_eq!(pre_tool_after[0]["hooks"][0]["command"], "rtk hook codex");
        assert!(after["hooks"].get("Stop").is_none());
    }

    #[test]
    fn legacy_grok_bridge_json_is_detected_for_reminder_without_deletion() {
        let directory = TestDir::new();
        let legacy = directory
            .0
            .join("grok-home")
            .join("hooks")
            .join(LEGACY_GROK_HOOK_FILE_NAME);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"{}").unwrap();

        let found = legacy_grok_hook_path_from(
            Some(directory.0.join("grok-home").into_os_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(found, legacy);
        assert!(found.exists());
    }
}
