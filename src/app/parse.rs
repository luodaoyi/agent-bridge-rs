use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, IsTerminal},
};

use anyhow::{Context, Result, bail};

use crate::{
    hooks::HooksTarget,
    install::InstallTarget,
    protocol::{CodexSandboxMode, Request, WaitCondition, WorkerKind},
};

use super::{Action, HooksAction, TerminalOptions};

pub(super) fn parse_args(arguments: Vec<OsString>) -> Result<Action> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        // No args: if both stdin and stdout are TTYs, open TUI; otherwise show help
        return if io::stdin().is_terminal() && io::stdout().is_terminal() {
            Ok(Action::Tui)
        } else {
            Ok(Action::Help)
        };
    };
    match command {
        "__hook" => parse_internal_hook(&arguments[1..]),
        "__server" => {
            ensure_no_arguments(&arguments[1..])?;
            Ok(Action::InternalServer)
        }
        #[cfg(windows)]
        "__windows-job-child" => {
            if arguments.len() < 4 {
                bail!("internal Windows job child requires event, PID report, and program names");
            }
            Ok(Action::InternalWindowsJobChild(arguments[1..].to_vec()))
        }
        "hooks" => parse_hooks(&arguments[1..]),
        "server" => parse_server(&arguments[1..]),
        "create" => parse_create(&arguments[1..]),
        "list" => {
            ensure_no_arguments(&arguments[1..])?;
            rpc(Request::List, true)
        }
        "heartbeat" => {
            ensure_no_arguments(&arguments[1..])?;
            rpc(Request::Heartbeat, true)
        }
        "close-client" | "close-codex" => {
            ensure_no_arguments(&arguments[1..])?;
            rpc(Request::CloseClient, true)
        }
        "show" => {
            let options = parse_options(&arguments[1..], &["--session"])?;
            rpc(
                Request::Show {
                    session: required(&options, "--session")?.to_owned(),
                },
                true,
            )
        }
        "read" => parse_read(&arguments[1..]),
        "send" => parse_send(&arguments[1..]),
        "write" => parse_write(&arguments[1..]),
        "resize" => parse_resize(&arguments[1..]),
        "wait" => parse_wait(&arguments[1..]),
        "close" => {
            let options = parse_options(&arguments[1..], &["--session"])?;
            rpc(
                Request::Close {
                    session: required(&options, "--session")?.to_owned(),
                },
                true,
            )
        }
        "terminal" => parse_terminal(&arguments[1..]),
        "ui" => {
            ensure_no_arguments(&arguments[1..])?;
            Ok(Action::Tui)
        }
        "install" | "apply" => parse_install_action(&arguments[1..], false),
        "status" => {
            if arguments.len() > 1 && arguments[1].to_str() == Some("install") {
                ensure_no_arguments(&arguments[2..])?;
                Ok(Action::InstallStatus)
            } else {
                bail!("unknown status subcommand; did you mean 'status install'?")
            }
        }
        "uninstall" | "remove" => parse_install_action(&arguments[1..], true),
        "doctor" | "--doctor" => parse_doctor(&arguments[1..]),
        "help" | "--help" | "-h" => {
            ensure_no_arguments(&arguments[1..])?;
            Ok(Action::Help)
        }
        "--version" | "-V" => {
            ensure_no_arguments(&arguments[1..])?;
            Ok(Action::Version)
        }
        other => bail!("unknown command: {other}"),
    }
}

fn parse_internal_hook(arguments: &[OsString]) -> Result<Action> {
    if arguments.is_empty() {
        return Ok(Action::InternalHook(WorkerKind::Grok));
    }
    let options = parse_options(arguments, &["--worker", "--protocol"])?;
    if let Some(protocol) = options.get("--protocol") {
        let _: u32 = protocol
            .parse()
            .with_context(|| format!("invalid value for --protocol: {protocol}"))?;
    }
    let worker = match options.get("--worker") {
        Some(value) => value.parse()?,
        None => WorkerKind::Grok,
    };
    Ok(Action::InternalHook(worker))
}

fn parse_install_action(arguments: &[OsString], uninstall: bool) -> Result<Action> {
    let options = parse_options(arguments, &["--target"])?;
    let target = match options.get("--target") {
        Some(value) => value.parse()?,
        None => InstallTarget::All,
    };
    Ok(if uninstall {
        Action::Uninstall(target)
    } else {
        Action::Install(target)
    })
}

fn parse_doctor(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(arguments, &["--worker"])?;
    let target = match options.get("--worker") {
        Some(value) => value.parse()?,
        None => HooksTarget::All,
    };
    Ok(Action::Doctor(target))
}

fn parse_hooks(arguments: &[OsString]) -> Result<Action> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        bail!("hooks requires install, status, or uninstall");
    };
    let options = parse_options(&arguments[1..], &["--worker"])?;
    let target = match options.get("--worker") {
        Some(value) => value.parse()?,
        None => HooksTarget::All,
    };
    let action = match command {
        "install" => HooksAction::Install,
        "status" => HooksAction::Status,
        "uninstall" => HooksAction::Uninstall,
        other => bail!("unknown hooks command: {other}"),
    };
    Ok(Action::Hooks { action, target })
}

fn parse_server(arguments: &[OsString]) -> Result<Action> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        bail!("server requires start, run, status, stop, or ui");
    };
    ensure_no_arguments(&arguments[1..])?;
    match command {
        "start" => rpc(Request::ServerStatus, true),
        "run" => Ok(Action::InternalServer),
        "status" => rpc(Request::ServerStatus, false),
        "stop" => rpc(Request::ServerStop, false),
        "ui" => Ok(Action::ServerUi),
        other => bail!("unknown server command: {other}"),
    }
}

fn parse_create(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(
        arguments,
        &[
            "--worker",
            "--cwd",
            "--prompt",
            "--model",
            "--owner",
            "--always-approve",
            "--sandbox",
            "--dangerously-bypass",
        ],
    )?;
    let worker: WorkerKind = required(&options, "--worker")?.parse()?;
    let sandbox = options
        .get("--sandbox")
        .map(|value| value.parse::<CodexSandboxMode>())
        .transpose()?;
    let dangerously_bypass = options.contains_key("--dangerously-bypass");
    if worker == WorkerKind::Grok && (sandbox.is_some() || dangerously_bypass) {
        bail!("--sandbox and --dangerously-bypass are only supported when --worker codex");
    }
    let cwd = match options.get("--cwd") {
        Some(cwd) => cwd.clone(),
        None => env::current_dir()
            .context("failed to determine the current working directory")?
            .to_string_lossy()
            .into_owned(),
    };
    rpc(
        Request::Create {
            worker,
            cwd,
            prompt: options.get("--prompt").cloned(),
            model: options.get("--model").cloned(),
            owner: options.get("--owner").cloned().or_else(default_owner),
            always_approve: options.contains_key("--always-approve"),
            sandbox,
            dangerously_bypass,
        },
        true,
    )
}

fn parse_terminal(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(
        arguments,
        &[
            "--session",
            "--worker",
            "--cwd",
            "--prompt",
            "--model",
            "--owner",
            "--always-approve",
            "--sandbox",
            "--dangerously-bypass",
        ],
    )?;
    let session = options.get("--session").cloned();
    if session.is_some()
        && [
            "--worker",
            "--cwd",
            "--prompt",
            "--model",
            "--owner",
            "--always-approve",
            "--sandbox",
            "--dangerously-bypass",
        ]
        .iter()
        .any(|name| options.contains_key(*name))
    {
        bail!("--session cannot be combined with session creation options");
    }
    let worker = if session.is_some() {
        None
    } else {
        Some(required(&options, "--worker")?.parse::<WorkerKind>()?)
    };
    let sandbox = options
        .get("--sandbox")
        .map(|value| value.parse::<CodexSandboxMode>())
        .transpose()?;
    let dangerously_bypass = options.contains_key("--dangerously-bypass");
    if worker == Some(WorkerKind::Grok) && (sandbox.is_some() || dangerously_bypass) {
        bail!("--sandbox and --dangerously-bypass are only supported when --worker codex");
    }
    Ok(Action::Terminal(TerminalOptions {
        session,
        worker,
        cwd: options.get("--cwd").cloned(),
        prompt: options.get("--prompt").cloned(),
        model: options.get("--model").cloned(),
        owner: options.get("--owner").cloned().or_else(default_owner),
        always_approve: options.contains_key("--always-approve"),
        sandbox,
        dangerously_bypass,
    }))
}

fn default_owner() -> Option<String> {
    [
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "ANTIGRAVITY_CONVERSATION_ID",
        "ANTIGRAVITY_TRAJECTORY_ID",
    ]
    .into_iter()
    .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
}

fn parse_read(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(
        arguments,
        &["--session", "--cursor", "--limit", "--wait-ms"],
    )?;
    rpc(
        Request::Read {
            session: required(&options, "--session")?.to_owned(),
            cursor: parse_number(&options, "--cursor")?,
            limit: parse_number(&options, "--limit")?,
            wait_ms: parse_number(&options, "--wait-ms")?,
        },
        true,
    )
}

fn parse_send(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(arguments, &["--session", "--text", "--interrupt"])?;
    let input = if options.contains_key("--interrupt") {
        if options.contains_key("--text") {
            bail!("--interrupt cannot be combined with --text");
        }
        "\u{3}".to_owned()
    } else {
        required(&options, "--text")?.to_owned()
    };
    rpc(
        Request::Send {
            session: required(&options, "--session")?.to_owned(),
            input,
        },
        true,
    )
}

fn parse_write(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(arguments, &["--session", "--data-base64"])?;
    rpc(
        Request::Write {
            session: required(&options, "--session")?.to_owned(),
            data_base64: required(&options, "--data-base64")?.to_owned(),
        },
        true,
    )
}

fn parse_resize(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(arguments, &["--session", "--cols", "--rows"])?;
    rpc(
        Request::Resize {
            session: required(&options, "--session")?.to_owned(),
            cols: parse_required_number(&options, "--cols")?,
            rows: parse_required_number(&options, "--rows")?,
        },
        true,
    )
}

fn parse_wait(arguments: &[OsString]) -> Result<Action> {
    let options = parse_options(arguments, &["--session", "--for", "--timeout-ms"])?;
    let condition = match required(&options, "--for")? {
        "tui-idle" => WaitCondition::TuiIdle,
        "exit" => WaitCondition::Exit,
        value => bail!("unsupported wait condition: {value}"),
    };
    rpc(
        Request::Wait {
            session: required(&options, "--session")?.to_owned(),
            for_condition: condition,
            timeout_ms: parse_number(&options, "--timeout-ms")?,
        },
        true,
    )
}

fn rpc(request: Request, auto_start: bool) -> Result<Action> {
    Ok(Action::Rpc {
        request,
        auto_start,
    })
}

fn parse_options(arguments: &[OsString], allowed: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut options = BTreeMap::new();
    let mut index = 0;
    while index < arguments.len() {
        let name = arguments[index]
            .to_str()
            .context("command options must be valid Unicode")?;
        if !allowed.contains(&name) {
            bail!("unknown option: {name}");
        }
        if options.contains_key(name) {
            bail!("option was provided more than once: {name}");
        }
        if matches!(
            name,
            "--always-approve" | "--dangerously-bypass" | "--interrupt"
        ) {
            options.insert(name.to_owned(), String::new());
            index += 1;
            continue;
        }
        let value = arguments
            .get(index + 1)
            .with_context(|| format!("{name} requires a value"))?
            .to_str()
            .with_context(|| format!("{name} must be valid Unicode"))?;
        if value.is_empty() {
            bail!("{name} requires a non-empty value");
        }
        options.insert(name.to_owned(), value.to_owned());
        index += 2;
    }
    Ok(options)
}

fn required<'a>(options: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    options
        .get(name)
        .map(String::as_str)
        .with_context(|| format!("missing required option: {name}"))
}

fn parse_number<T>(options: &BTreeMap<String, String>, name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    options
        .get(name)
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|error| anyhow::anyhow!("invalid value for {name}: {value}: {error}"))
        })
        .transpose()
}

fn parse_required_number<T>(options: &BTreeMap<String, String>, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = required(options, name)?;
    value
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("invalid value for {name}: {value}: {error}"))
}

fn ensure_no_arguments(arguments: &[OsString]) -> Result<()> {
    if arguments.is_empty() {
        Ok(())
    } else {
        bail!("unexpected arguments")
    }
}
