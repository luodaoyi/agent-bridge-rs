mod gui_fonts;
mod hooks;
mod install;
mod protocol;
mod server;
mod session;
mod terminal_gui;
mod transport;
mod tui;
mod version_check;
mod worker;

mod app {
    use std::{
        collections::BTreeMap,
        env,
        ffi::{OsStr, OsString},
        io::{self, IsTerminal, Write},
        process::Command,
    };

    use anyhow::{Context, Result, bail};
    use serde::Serialize;

    use crate::{
        hooks::{self, HooksTarget},
        install::{self, InstallTarget},
        protocol::{
            CodexSandboxMode, Request, ResponseEnvelope, ResponseResult, WaitCondition, WorkerKind,
        },
        server, session, terminal_gui, transport, tui, worker,
    };

    struct TerminalOptions {
        session: Option<String>,
        worker: Option<WorkerKind>,
        cwd: Option<String>,
        prompt: Option<String>,
        model: Option<String>,
        owner: Option<String>,
        always_approve: bool,
        sandbox: Option<CodexSandboxMode>,
        dangerously_bypass: bool,
    }

    enum Action {
        Rpc {
            request: Request,
            auto_start: bool,
        },
        Terminal(TerminalOptions),
        Hooks {
            action: HooksAction,
            target: HooksTarget,
        },
        ServerUi,
        Doctor(HooksTarget),
        Help,
        Version,
        Tui,
        Install(InstallTarget),
        InstallStatus,
        Uninstall(InstallTarget),
        InternalHook(WorkerKind),
        InternalServer,
        #[cfg(windows)]
        InternalWindowsJobChild(Vec<OsString>),
    }

    enum HooksAction {
        Install,
        Status,
        Uninstall,
    }

    pub(super) fn main() {
        let exit_code = match run() {
            Ok(exit_code) => exit_code,
            Err(error) => {
                eprintln!("agent-bridge: {error:#}");
                1
            }
        };
        std::process::exit(exit_code);
    }

    fn run() -> Result<i32> {
        match parse_args(env::args_os().skip(1).collect())? {
            Action::Rpc {
                request,
                auto_start,
            } => {
                let response = transport::call(request, auto_start)?;
                write_json(&response)?;
                Ok(if response.ok { 0 } else { 1 })
            }
            Action::Terminal(options) => {
                let session = terminal_session(options)?;
                terminal_gui::run(session)?;
                Ok(0)
            }
            Action::Hooks { action, target } => {
                let status = match action {
                    HooksAction::Install => hooks::install_target(target)?,
                    HooksAction::Status => hooks::status_target(target)?,
                    HooksAction::Uninstall => hooks::uninstall_target(target)?,
                };
                write_json(&status)?;
                Ok(0)
            }
            Action::ServerUi => server_ui(),
            Action::Doctor(target) => doctor(target),
            Action::Tui => {
                tui::run()?;
                Ok(0)
            }
            Action::Install(target) => match install::apply(target) {
                Ok(result) => {
                    eprintln!("安装成功：{}", result.display());
                    Ok(0)
                }
                Err(error) => {
                    eprintln!("安装失败：{error:#}");
                    Ok(1)
                }
            },
            Action::InstallStatus => {
                let paths = install::Paths::discover()?;
                let status = install::status(&paths)?;
                write_json(&status)?;
                Ok(0)
            }
            Action::Uninstall(target) => match install::uninstall(target) {
                Ok(true) => {
                    eprintln!("卸载成功：已移除 agent-bridge Hooks 配置");
                    Ok(0)
                }
                Ok(false) => {
                    eprintln!("未找到本工具的 Hooks 配置");
                    Ok(0)
                }
                Err(error) => {
                    eprintln!("卸载失败：{error:#}");
                    Ok(1)
                }
            },
            Action::Help => {
                print_help();
                Ok(0)
            }
            Action::Version => {
                println!("agent-bridge {}", env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            Action::InternalHook(worker) => {
                hooks::run_ingress_fail_open(worker);
                Ok(0)
            }
            Action::InternalServer => {
                server::run()?;
                Ok(0)
            }
            #[cfg(windows)]
            Action::InternalWindowsJobChild(arguments) => session::run_windows_job_child(arguments),
        }
    }

    fn parse_args(arguments: Vec<OsString>) -> Result<Action> {
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
                    bail!(
                        "internal Windows job child requires event, PID report, and program names"
                    );
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
            bail!("server requires start, status, stop, or ui");
        };
        ensure_no_arguments(&arguments[1..])?;
        match command {
            "start" => rpc(Request::ServerStatus, true),
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

    fn terminal_session(options: TerminalOptions) -> Result<String> {
        if let Some(session) = options.session {
            let response = transport::call(
                Request::Show {
                    session: session.clone(),
                },
                true,
            )?;
            ensure_success(&response)?;
            return Ok(session);
        }

        let worker = options
            .worker
            .context("missing required option: --worker")?;
        let cwd = match options.cwd {
            Some(cwd) => cwd,
            None => env::current_dir()
                .context("failed to determine the current working directory")?
                .to_string_lossy()
                .into_owned(),
        };
        let response = transport::call(
            Request::Create {
                worker,
                cwd,
                prompt: options.prompt,
                model: options.model,
                owner: options.owner,
                always_approve: options.always_approve,
                sandbox: options.sandbox,
                dangerously_bypass: options.dangerously_bypass,
            },
            true,
        )?;
        ensure_success(&response)?;
        match response.result {
            Some(ResponseResult::Session(state)) => Ok(state.session),
            _ => bail!("runtime returned an unexpected create response"),
        }
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

    fn server_ui() -> Result<i32> {
        let response = transport::call(Request::ServerStatus, true)?;
        ensure_success(&response)?;
        match response.result {
            Some(ResponseResult::ServerInfo(info)) => match info.web_url {
                Some(url) => {
                    open_browser(&url)?;
                    eprintln!("agent-bridge WebUI: {url}");
                    Ok(0)
                }
                None => bail!("runtime WebUI is unavailable; check server stderr"),
            },
            _ => bail!("runtime returned an unexpected server status response"),
        }
    }

    fn open_browser(url: &str) -> Result<()> {
        #[cfg(windows)]
        let mut command = Command::new("explorer.exe");
        #[cfg(target_os = "macos")]
        let mut command = Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = Command::new("xdg-open");
        #[cfg(not(any(windows, unix)))]
        bail!("opening a browser is unsupported on this platform; visit {url}");
        command
            .arg(url)
            .spawn()
            .context("failed to open the Runtime WebUI in the default browser")?;
        Ok(())
    }

    fn ensure_success(response: &ResponseEnvelope) -> Result<()> {
        if response.ok {
            return Ok(());
        }
        match &response.error {
            Some(error) => bail!("{}: {}", error.code, error.message),
            None => bail!("runtime returned an unsuccessful response without an error"),
        }
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

    fn write_json(value: &impl Serialize) -> Result<()> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        serde_json::to_writer(&mut output, value).context("failed to write JSON response")?;
        output
            .write_all(b"\n")
            .context("failed to finish JSON response")
    }

    fn probe_worker_version(worker_kind: WorkerKind) -> bool {
        let bin = worker::resolve_worker_bin(worker_kind);
        let env_hint = match worker_kind {
            WorkerKind::Grok => "GROK_BIN",
            WorkerKind::Codex => "CODEX_BIN",
        };
        match Command::new(&bin).arg("--version").output() {
            Ok(output) if output.status.success() => {
                print!(
                    "[{}] {}",
                    worker_kind.as_str(),
                    String::from_utf8_lossy(&output.stdout)
                );
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                true
            }
            Ok(output) => {
                eprintln!(
                    "[{}] version check failed with {}: {}",
                    worker_kind.as_str(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                false
            }
            Err(error) => {
                eprintln!(
                    "[{}] unavailable ({}): {error}; set {env_hint} if installed elsewhere",
                    worker_kind.as_str(),
                    OsStr::new(&bin).to_string_lossy()
                );
                false
            }
        }
    }

    fn doctor(target: HooksTarget) -> Result<i32> {
        let ok = match target {
            HooksTarget::Grok => probe_worker_version(WorkerKind::Grok),
            HooksTarget::Codex => probe_worker_version(WorkerKind::Codex),
            HooksTarget::All => {
                let grok_ok = probe_worker_version(WorkerKind::Grok);
                let codex_ok = probe_worker_version(WorkerKind::Codex);
                grok_ok || codex_ok
            }
        };
        if let Ok(hook_status) = hooks::status_target(target) {
            write_json(&hook_status)?;
        }
        match transport::call(Request::ServerStatus, false) {
            Ok(response) => write_json(&response)?,
            Err(_) => println!("runtime_server=stopped"),
        }
        if !ok {
            bail!("requested worker CLI was not available");
        }
        Ok(0)
    }

    fn print_help() {
        println!(
            "agent-bridge {}\n\nUSAGE:\n  agent-bridge ui                                   # 打开中文 TUI 安装配置界面\n  agent-bridge install [--target all|codex|agy]     # 部署 Codex Skill / AGY Plugin 与 Hooks\n  agent-bridge status install                       # 显示安装状态（JSON）\n  agent-bridge uninstall [--target all|codex|agy]   # 移除 agent-bridge Hooks 配置\n  agent-bridge hooks install|status|uninstall [--worker all|grok|codex]\n  agent-bridge server start|status|stop|ui\n  agent-bridge create --worker grok|codex [--cwd <path>] [--prompt <text>] [--model <model>] [--owner <title>] [--always-approve] [--sandbox read-only|workspace-write|danger-full-access] [--dangerously-bypass]\n  agent-bridge list\n  agent-bridge heartbeat\n  agent-bridge close-client                         # 兼容别名: close-codex\n  agent-bridge show --session <handle>\n  agent-bridge read --session <handle> [--cursor <n>] [--limit <bytes>] [--wait-ms <n>]\n  agent-bridge send --session <handle> (--text <text> | --interrupt)\n  agent-bridge write --session <handle> --data-base64 <base64>\n  agent-bridge resize --session <handle> --cols <n> --rows <n>\n  agent-bridge wait --session <handle> --for tui-idle|exit [--timeout-ms <n>]\n  agent-bridge close --session <handle>\n  agent-bridge terminal --session <handle>\n  agent-bridge terminal --worker grok|codex [--cwd <path>] [--prompt <text>] [--model <model>] [--owner <title>] [--always-approve] [--sandbox ...] [--dangerously-bypass]\n  agent-bridge doctor [--worker all|grok|codex]\n\nNPM GLOBAL 安装：\n  pnpm add -g agent-bridge-rs@latest\n  agent-bridge                        # 自动打开 TUI\n\n支持双宿主（Codex / Antigravity）与双执行器（Grok Build / Codex CLI）。`create` 与创建态 `terminal` 必须显式指定 `--worker grok|codex`。RPC 自动识别 CODEX_THREAD_ID / CODEX_SESSION_ID / ANTIGRAVITY_CONVERSATION_ID 租约；`close-client`（别名 `close-codex`）只关闭当前宿主会话派生的子进程。默认 WebUI 端口 127.0.0.1:47654。",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_wait_and_interrupt_commands() {
            assert!(matches!(
                parse_args(vec!["heartbeat".into()]).unwrap(),
                Action::Rpc {
                    request: Request::Heartbeat,
                    ..
                }
            ));
            assert!(matches!(
                parse_args(vec!["close-codex".into()]).unwrap(),
                Action::Rpc {
                    request: Request::CloseClient,
                    ..
                }
            ));
            assert!(matches!(
                parse_args(vec!["close-client".into()]).unwrap(),
                Action::Rpc {
                    request: Request::CloseClient,
                    ..
                }
            ));
            let action = parse_args(vec![
                "wait".into(),
                "--session".into(),
                "gbt-1".into(),
                "--for".into(),
                "tui-idle".into(),
            ])
            .unwrap();
            assert!(matches!(
                action,
                Action::Rpc {
                    request: Request::Wait {
                        for_condition: WaitCondition::TuiIdle,
                        ..
                    },
                    ..
                }
            ));

            let action = parse_args(vec![
                "send".into(),
                "--session".into(),
                "gbt-1".into(),
                "--interrupt".into(),
            ])
            .unwrap();
            assert!(matches!(
                action,
                Action::Rpc {
                    request: Request::Send { input, .. },
                    ..
                } if input == "\u{3}"
            ));
        }

        #[test]
        fn create_and_terminal_create_require_explicit_worker_and_enforce_codex_only_flags() {
            assert!(parse_args(vec!["create".into(), "--cwd".into(), ".".into()]).is_err());
            assert!(parse_args(vec!["terminal".into(), "--prompt".into(), "hi".into()]).is_err());
            assert!(
                parse_args(vec![
                    "create".into(),
                    "--worker".into(),
                    "grok".into(),
                    "--sandbox".into(),
                    "workspace-write".into(),
                ])
                .is_err()
            );
            assert!(
                parse_args(vec![
                    "create".into(),
                    "--worker".into(),
                    "grok".into(),
                    "--dangerously-bypass".into(),
                ])
                .is_err()
            );

            let codex_create = parse_args(vec![
                "create".into(),
                "--worker".into(),
                "codex".into(),
                "--cwd".into(),
                ".".into(),
                "--always-approve".into(),
                "--sandbox".into(),
                "read-only".into(),
            ])
            .unwrap();
            assert!(matches!(
                codex_create,
                Action::Rpc {
                    request: Request::Create {
                        worker: WorkerKind::Codex,
                        always_approve: true,
                        sandbox: Some(CodexSandboxMode::ReadOnly),
                        dangerously_bypass: false,
                        ..
                    },
                    ..
                }
            ));
        }

        #[test]
        fn parses_hook_management_and_hidden_ingress_commands() {
            assert!(matches!(
                parse_args(vec!["hooks".into(), "install".into()]).unwrap(),
                Action::Hooks {
                    action: HooksAction::Install,
                    target: HooksTarget::All,
                }
            ));
            assert!(matches!(
                parse_args(vec![
                    "hooks".into(),
                    "status".into(),
                    "--worker".into(),
                    "codex".into(),
                ])
                .unwrap(),
                Action::Hooks {
                    action: HooksAction::Status,
                    target: HooksTarget::Codex,
                }
            ));
            assert!(matches!(
                parse_args(vec!["hooks".into(), "uninstall".into()]).unwrap(),
                Action::Hooks {
                    action: HooksAction::Uninstall,
                    target: HooksTarget::All,
                }
            ));
            assert!(matches!(
                parse_args(vec!["__hook".into()]).unwrap(),
                Action::InternalHook(WorkerKind::Grok)
            ));
            assert!(matches!(
                parse_args(vec![
                    "__hook".into(),
                    "--worker".into(),
                    "codex".into(),
                    "--protocol".into(),
                    "5".into(),
                ])
                .unwrap(),
                Action::InternalHook(WorkerKind::Codex)
            ));
            assert!(parse_args(vec!["__hook".into(), "extra".into()]).is_err());
        }

        #[cfg(windows)]
        #[test]
        fn parses_internal_windows_job_child_without_shell_joining() {
            let action = parse_args(vec![
                "__windows-job-child".into(),
                "Local\\gate".into(),
                "Local\\pid".into(),
                "C:\\Program Files\\Grok\\grok.exe".into(),
                "--model".into(),
                "deepseek-v4-flash".into(),
            ])
            .unwrap();
            assert!(matches!(
                action,
                Action::InternalWindowsJobChild(arguments)
                    if arguments == [
                        OsString::from("Local\\gate"),
                        OsString::from("Local\\pid"),
                        OsString::from("C:\\Program Files\\Grok\\grok.exe"),
                        OsString::from("--model"),
                        OsString::from("deepseek-v4-flash"),
                    ]
            ));
            assert!(parse_args(vec!["__windows-job-child".into()]).is_err());
        }

        #[test]
        fn rejects_duplicate_and_conflicting_options() {
            assert!(
                parse_args(vec![
                    "show".into(),
                    "--session".into(),
                    "one".into(),
                    "--session".into(),
                    "two".into(),
                ])
                .is_err()
            );
            assert!(
                parse_args(vec![
                    "send".into(),
                    "--session".into(),
                    "one".into(),
                    "--text".into(),
                    "hello".into(),
                    "--interrupt".into(),
                ])
                .is_err()
            );
            assert!(
                parse_args(vec![
                    "terminal".into(),
                    "--session".into(),
                    "one".into(),
                    "--prompt".into(),
                    "new task".into(),
                ])
                .is_err()
            );
        }

        #[test]
        fn parses_terminal_attach_and_create_modes() {
            let attach =
                parse_args(vec!["terminal".into(), "--session".into(), "gbt-1".into()]).unwrap();
            assert!(matches!(
                attach,
                Action::Terminal(TerminalOptions {
                    session: Some(session),
                    ..
                }) if session == "gbt-1"
            ));

            let create = parse_args(vec![
                "terminal".into(),
                "--worker".into(),
                "codex".into(),
                "--prompt".into(),
                "修复中文".into(),
                "--always-approve".into(),
            ])
            .unwrap();
            assert!(matches!(
                create,
                Action::Terminal(TerminalOptions {
                    session: None,
                    worker: Some(WorkerKind::Codex),
                    prompt: Some(prompt),
                    always_approve: true,
                    ..
                }) if prompt == "修复中文"
            ));
        }
    }
}

fn main() {
    app::main();
}
