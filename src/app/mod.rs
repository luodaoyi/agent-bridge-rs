mod parse;

#[cfg(test)]
mod tests;

#[cfg(windows)]
use std::ffi::OsString;
use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    hooks::{self, HooksTarget},
    install::{self, InstallTarget},
    protocol::{CodexSandboxMode, Request, ResponseEnvelope, ResponseResult, WorkerKind},
    server, terminal_gui, transport, tui, worker,
};

use parse::parse_args;

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
        Action::InternalWindowsJobChild(arguments) => {
            crate::session::run_windows_job_child(arguments)
        }
    }
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
    let mut probe_command = worker::build_probe_command(OsStr::new(&bin));
    match probe_command.arg("--version").output() {
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
        "agent-bridge {}\n\nUSAGE:\n  agent-bridge ui                                   # 打开中文 TUI 安装配置界面\n  agent-bridge install [--target all|codex|agy]     # 部署 Codex Skill / AGY Plugin 与 Hooks\n  agent-bridge status install                       # 显示安装状态（JSON）\n  agent-bridge uninstall [--target all|codex|agy]   # 移除 agent-bridge Hooks 配置\n  agent-bridge hooks install|status|uninstall [--worker all|grok|codex]\n  agent-bridge server start|run|status|stop|ui\n  agent-bridge create --worker grok|codex [--cwd <path>] [--prompt <text>] [--model <model>] [--owner <title>] [--always-approve] [--sandbox read-only|workspace-write|danger-full-access] [--dangerously-bypass]\n  agent-bridge list\n  agent-bridge heartbeat\n  agent-bridge close-client                         # 兼容别名: close-codex\n  agent-bridge show --session <handle>\n  agent-bridge read --session <handle> [--cursor <n>] [--limit <bytes>] [--wait-ms <n>]\n  agent-bridge send --session <handle> (--text <text> | --interrupt)\n  agent-bridge write --session <handle> --data-base64 <base64>\n  agent-bridge resize --session <handle> --cols <n> --rows <n>\n  agent-bridge wait --session <handle> --for tui-idle|exit [--timeout-ms <n>]\n  agent-bridge close --session <handle>\n  agent-bridge terminal --session <handle>\n  agent-bridge terminal --worker grok|codex [--cwd <path>] [--prompt <text>] [--model <model>] [--owner <title>] [--always-approve] [--sandbox ...] [--dangerously-bypass]\n  agent-bridge doctor [--worker all|grok|codex]\n\nNPM GLOBAL 安装：\n  pnpm add -g agent-bridge-rs@latest\n  agent-bridge                        # 自动打开 TUI\n\n支持双宿主（Codex / Antigravity）与双执行器（Grok Build / Codex CLI）。`create` 与创建态 `terminal` 必须显式指定 `--worker grok|codex`。RPC 自动识别 CODEX_THREAD_ID / CODEX_SESSION_ID / ANTIGRAVITY_CONVERSATION_ID 租约；`close-client`（别名 `close-codex`）只关闭当前宿主会话派生的子进程。默认 WebUI 端口 127.0.0.1:47654。",
        env!("CARGO_PKG_VERSION")
    );
}
