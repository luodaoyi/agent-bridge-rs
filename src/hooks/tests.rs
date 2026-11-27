use std::{
    io::Cursor,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use super::config::decode_powershell_encoded_command;
use super::config::{load_document, write_document};
use super::*;
use serde_json::json;
use std::fs;

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
        include_str!("../../hooks/windows/agent-bridge-grok.json"),
        include_str!("../../hooks/unix/agent-bridge-grok.json"),
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
        include_str!("../../hooks/windows/agent-bridge-codex.json"),
        include_str!("../../hooks/unix/agent-bridge-codex.json"),
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
        pre_tool
            .iter()
            .any(|g| { g["matcher"] == "Bash" && g["hooks"][0]["command"] == "rtk hook codex" })
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

#[test]
fn status_at_candidates_matches_deployed_binary_when_caller_differs() {
    let directory = TestDir::new();
    let grok_path = directory.0.join("grok-hooks.json");
    let codex_path = directory.0.join("codex-hooks.json");
    let caller_bin = directory.0.join("npm").join("agent-bridge");
    let deployed_bin = directory.0.join("plugin").join("agent-bridge");
    let caller_grok = hook_command(WorkerKind::Grok, &caller_bin).unwrap();
    let deployed_grok = hook_command(WorkerKind::Grok, &deployed_bin).unwrap();
    let caller_codex = hook_command(WorkerKind::Codex, &caller_bin).unwrap();
    let deployed_codex = hook_command(WorkerKind::Codex, &deployed_bin).unwrap();

    assert!(
        install_at(WorkerKind::Grok, &grok_path, &deployed_grok)
            .unwrap()
            .installed
    );
    assert!(
        install_at(WorkerKind::Codex, &codex_path, &deployed_codex)
            .unwrap()
            .installed
    );

    let grok_status =
        status_at_candidates(WorkerKind::Grok, &grok_path, &[caller_grok, deployed_grok]).unwrap();
    assert!(grok_status.installed);
    assert_eq!(grok_status.events_found, grok_status.events_expected);

    let codex_status = status_at_candidates(
        WorkerKind::Codex,
        &codex_path,
        &[caller_codex, deployed_codex],
    )
    .unwrap();
    assert!(codex_status.installed);
    assert_eq!(codex_status.events_found, codex_status.events_expected);
}

#[test]
fn status_at_candidates_rejects_mixed_binaries_as_not_installed() {
    let directory = TestDir::new();
    let codex_path = directory.0.join("codex-hooks.json");
    let caller_bin = directory.0.join("npm").join("agent-bridge");
    let deployed_bin = directory.0.join("plugin").join("agent-bridge");
    let caller_codex = hook_command(WorkerKind::Codex, &caller_bin).unwrap();
    let deployed_codex = hook_command(WorkerKind::Codex, &deployed_bin).unwrap();

    install_at(WorkerKind::Codex, &codex_path, &deployed_codex).unwrap();
    let mut document = load_document(&codex_path).unwrap().unwrap();
    document["hooks"]["Stop"][0]["hooks"][0]["command"] = Value::String(caller_codex.clone());
    write_document(&codex_path, &document).unwrap();

    let status = status_at_candidates(
        WorkerKind::Codex,
        &codex_path,
        &[caller_codex, deployed_codex],
    )
    .unwrap();
    assert!(!status.installed);
    assert!(status.events_found < status.events_expected);
}

#[test]
fn loopback_hook_url_parser_accepts_only_loopback_literal_ip_and_exact_route() {
    assert!(http_fallback::parse_loopback_hook_url("http://127.0.0.1:47654/api/hooks").is_ok());
    assert!(http_fallback::parse_loopback_hook_url("http://[::1]:47654/api/hooks").is_ok());
    for bad in [
        "https://127.0.0.1:47654/api/hooks",
        "http://localhost:47654/api/hooks",
        "http://192.168.1.20:47654/api/hooks",
        "http://127.0.0.1:47654/api/sessions",
        "http://127.0.0.1:47654/api/hooks?extra=1",
    ] {
        assert!(
            http_fallback::parse_loopback_hook_url(bad).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn deliver_hook_event_falls_back_to_loopback_http_when_ipc_fails() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    let dir = tempfile::tempdir().unwrap();
    let endpoint_path = dir.path().join("hook-endpoint.json");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let record = http_fallback::HookEndpointState {
        url: format!("http://{addr}/api/hooks"),
        token: "0123456789abcdef0123456789abcdef".to_owned(),
        pid: std::process::id(),
    };
    fs::write(&endpoint_path, serde_json::to_vec(&record).unwrap()).unwrap();

    let server = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut raw = String::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            raw.push_str(&line);
            if let Some(val) = line
                .strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
            {
                content_length = val.trim().parse::<usize>().unwrap();
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).unwrap();
        raw.push_str(&String::from_utf8_lossy(&body));
        let inner = reader.into_inner();
        inner
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"accepted\":true}",
            )
            .unwrap();
        inner.flush().unwrap();
        raw
    });

    let event = HookEvent {
        kind: HookEventKind::Stop,
        cwd: Some(dir.path().display().to_string()),
        tool_name: None,
        message: None,
        notification_type: None,
        level: None,
    };
    let delivered = deliver_hook_event_with(
        WorkerKind::Codex,
        "01234567-89ab-4def-8123-456789abcdef".to_owned(),
        event,
        Some(&endpoint_path),
        |_| bail!("ipc unavailable"),
    );
    assert!(delivered);
    let received = server.join().unwrap();
    assert!(received.starts_with("POST /api/hooks HTTP/1.1\r\n"));
    assert!(received.contains("X-Agent-Bridge-Hook-Token: 0123456789abcdef0123456789abcdef"));
    assert!(received.contains("\"worker\":\"codex\""));
    assert!(received.contains("\"kind\":\"stop\""));
}
