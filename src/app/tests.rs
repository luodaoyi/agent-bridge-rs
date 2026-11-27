use super::*;
use crate::protocol::WaitCondition;

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
    let attach = parse_args(vec!["terminal".into(), "--session".into(), "gbt-1".into()]).unwrap();
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

#[test]
fn parses_server_run_foreground_command() {
    assert!(matches!(
        parse_args(vec!["server".into(), "run".into()]).unwrap(),
        Action::InternalServer
    ));
    assert!(parse_args(vec!["server".into(), "run".into(), "--extra".into()]).is_err());
}
