#[cfg(windows)]
use std::env;
use std::{fs, io, path::Path};

use anyhow::{Context, Result, bail};
#[cfg(any(windows, test))]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};

use super::{
    HOOK_TIMEOUT_SECONDS, HookInstallStatus, HookSpec, MANAGED_ENV_NAME, MANAGED_ENV_VALUE,
    PROTOCOL_ENV_NAME, PROTOCOL_ENV_VALUE, events_for,
};
use crate::protocol::WorkerKind;

#[cfg(windows)]
pub(super) fn hook_command(worker: WorkerKind, executable: &Path) -> Result<String> {
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
pub(super) fn hook_command(worker: WorkerKind, executable: &Path) -> Result<String> {
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

pub(super) fn install_at(
    worker: WorkerKind,
    path: &Path,
    command: &str,
) -> Result<HookInstallStatus> {
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

pub(super) fn status_at(
    worker: WorkerKind,
    path: &Path,
    command: &str,
) -> Result<HookInstallStatus> {
    status_at_candidates(worker, path, &[command.to_owned()])
}

pub(super) fn status_at_candidates(
    worker: WorkerKind,
    path: &Path,
    commands: &[String],
) -> Result<HookInstallStatus> {
    let Some(document) = load_document(path)? else {
        return Ok(empty_status(worker, path));
    };
    validate_document(&document)?;
    let Some(first) = commands.first() else {
        return Ok(empty_status(worker, path));
    };
    let mut best = inspect_document(worker, path, &document, first);
    if best.installed {
        return Ok(best);
    }
    for command in &commands[1..] {
        let candidate = inspect_document(worker, path, &document, command);
        if candidate.installed {
            return Ok(candidate);
        }
        if candidate.events_found > best.events_found {
            best = candidate;
        }
    }
    Ok(best)
}

pub(super) fn uninstall_at(
    worker: WorkerKind,
    path: &Path,
    command: &str,
) -> Result<HookInstallStatus> {
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

pub(super) fn load_document(path: &Path) -> Result<Option<Value>> {
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

pub(super) fn write_document(path: &Path, document: &Value) -> Result<()> {
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
    #[cfg(any(windows, test))]
    let encoded_matches =
        decode_powershell_encoded_command(command).is_some_and(|decoded| decoded.contains(&needle));
    #[cfg(not(any(windows, test)))]
    let encoded_matches = false;
    command.contains(&needle) || encoded_matches
}

#[cfg(any(windows, test))]
pub(super) fn decode_powershell_encoded_command(command: &str) -> Option<String> {
    let marker = " -EncodedCommand ";
    let (_, encoded) = command.split_once(marker)?;
    let encoded_token = encoded.split_whitespace().next()?;
    let bytes = BASE64.decode(encoded_token).ok()?;
    let (chunks, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let utf16 = chunks
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
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
