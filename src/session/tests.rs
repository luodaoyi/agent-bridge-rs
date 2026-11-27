use super::process::*;
use super::registry::{HookDiagnosticsTracker, SessionRegistry};
use super::web_events::*;
use super::*;
use std::fs;

const TEST_PROVIDER_SESSION_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

#[cfg(windows)]
const WINDOWS_PROCESS_TREE_ROLE_ENV: &str = "AGENT_BRIDGE_TEST_PROCESS_TREE_ROLE";
#[cfg(windows)]
const WINDOWS_PROCESS_TREE_DIR_ENV: &str = "AGENT_BRIDGE_TEST_PROCESS_TREE_DIR";
#[cfg(windows)]
const WINDOWS_PROCESS_TREE_ROOT_ENV: &str = "AGENT_BRIDGE_TEST_PROCESS_TREE_ROOT";
#[cfg(windows)]
const WINDOWS_PROCESS_TREE_GATE_ENV: &str = "AGENT_BRIDGE_TEST_PROCESS_TREE_GATE";
#[cfg(windows)]
const WINDOWS_PROCESS_TREE_HELPER_TEST: &str = "session::tests::windows_process_tree_helper";
#[cfg(windows)]
const WINDOWS_JOB_CHILD_HELPER_ENV: &str = "AGENT_BRIDGE_TEST_JOB_CHILD_HELPER";
#[cfg(windows)]
const WINDOWS_JOB_CHILD_GATE_ENV: &str = "AGENT_BRIDGE_TEST_JOB_CHILD_GATE";
#[cfg(windows)]
const WINDOWS_JOB_CHILD_PID_ENV: &str = "AGENT_BRIDGE_TEST_JOB_CHILD_PID";
#[cfg(windows)]
const WINDOWS_JOB_CHILD_HELPER_TEST: &str = "session::tests::windows_job_child_handshake_helper";
#[cfg(windows)]
const WINDOWS_PROCESS_TREE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(windows)]
struct WindowsProcessTreeCleanup {
    root: Option<std::process::Child>,
    directory: PathBuf,
    pids: Vec<u32>,
    armed: bool,
}

#[cfg(windows)]
impl WindowsProcessTreeCleanup {
    fn new(root: std::process::Child, directory: PathBuf) -> Self {
        let root_pid = root.id();
        Self {
            root: Some(root),
            directory,
            pids: vec![root_pid],
            armed: true,
        }
    }

    fn remember(&mut self, pids: &[u32]) {
        self.pids.extend_from_slice(pids);
        self.pids.sort_unstable();
        self.pids.dedup();
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.root.take();
        self.pids.clear();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessTreeCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for name in ["child.pid", "grandchild.pid"] {
            if let Ok(value) = fs::read_to_string(self.directory.join(name))
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                self.pids.push(pid);
            }
        }
        self.pids.sort_unstable();
        self.pids.dedup();
        for &pid in self.pids.iter().rev() {
            terminate_windows_test_process(pid);
        }
        if let Some(root) = self.root.as_mut() {
            let _ = root.kill();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(windows)]
fn terminate_windows_test_process(pid: u32) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if !handle.is_null() {
        unsafe {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

#[cfg(windows)]
fn spawn_windows_process_tree_helper(
    role: &str,
    directory: &Path,
    root_pid: u32,
    gate_name: Option<&std::ffi::OsStr>,
) -> std::process::Child {
    use std::process::{Command, Stdio};

    let mut command =
        Command::new(env::current_exe().expect("test executable must be discoverable"));
    command
        .args(["--exact", WINDOWS_PROCESS_TREE_HELPER_TEST, "--nocapture"])
        .env(WINDOWS_PROCESS_TREE_ROLE_ENV, role)
        .env(WINDOWS_PROCESS_TREE_DIR_ENV, directory)
        .env(WINDOWS_PROCESS_TREE_ROOT_ENV, root_pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(gate_name) = gate_name {
        command.env(WINDOWS_PROCESS_TREE_GATE_ENV, gate_name);
    }
    command
        .spawn()
        .expect("Windows process-tree helper must start")
}

#[cfg(windows)]
fn wait_for_windows_process_tree_ready(path: &Path) -> [u32; 3] {
    let deadline = Instant::now() + WINDOWS_PROCESS_TREE_TIMEOUT;
    let mut last_contents = String::new();
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            last_contents = contents;
            let pids = last_contents
                .split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
                .collect::<Vec<_>>();
            if let [root, child, grandchild] = pids.as_slice()
                && *root != 0
                && *child != 0
                && *grandchild != 0
            {
                return [*root, *child, *grandchild];
            }
        }
        assert!(
            Instant::now() < deadline,
            "Windows process tree did not become ready within {WINDOWS_PROCESS_TREE_TIMEOUT:?}; last ready contents: {last_contents:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn wait_for_windows_process_tree_exit(pids: [u32; 3]) {
    let deadline = Instant::now() + WINDOWS_PROCESS_TREE_TIMEOUT;
    loop {
        let states = pids.map(process_alive);
        if states.iter().all(|state| *state == ScopeAlive::Gone) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Windows process tree did not exit within {WINDOWS_PROCESS_TREE_TIMEOUT:?}: pids={pids:?}, states={states:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn wait_for_windows_intermediates_exit(pids: [u32; 2]) {
    let deadline = Instant::now() + WINDOWS_PROCESS_TREE_TIMEOUT;
    loop {
        let states = pids.map(process_alive);
        if states.iter().all(|state| *state == ScopeAlive::Gone) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "wrapper and short-lived child did not exit within {WINDOWS_PROCESS_TREE_TIMEOUT:?}: pids={pids:?}, states={states:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn wait_for_windows_marker(path: &Path) {
    let deadline = Instant::now() + WINDOWS_PROCESS_TREE_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "Windows process-tree marker did not appear within {WINDOWS_PROCESS_TREE_TIMEOUT:?}: {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn temporary_test_directory(label: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "grok-bridge-{label}-{}-{}",
        std::process::id(),
        generate_provider_session_id().unwrap()
    ))
}

fn hook_event(kind: HookEventKind) -> HookEvent {
    HookEvent {
        kind,
        cwd: None,
        tool_name: None,
        message: None,
        notification_type: None,
        level: None,
    }
}

fn test_session(phase: SessionPhase) -> Session {
    test_session_with_revision(phase, Arc::new(HostRevision::new()))
}

fn test_session_with_revision(phase: SessionPhase, host_revision: Arc<HostRevision>) -> Session {
    let cwd = canonical_directory(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let (writer_tx, _writer_rx) = sync_channel(1);
    let terminal = phase_is_terminal(phase);
    Session {
        scope_id: 0,
        inner: Mutex::new(SessionInner {
            session: "gbt-test".to_owned(),
            worker: WorkerKind::Grok,
            client_kind: None,
            owner: Some("test-owner".to_owned()),
            client_session_id: None,
            client_lease: None,
            orphan_policy: OrphanPolicy {
                lease_ms: 120_000,
                grace_ms: 600_000,
            },
            phase,
            phase_changed_at_ms: 1,
            cwd: cwd.to_string_lossy().into_owned(),
            model: Some("grok-test".to_owned()),
            always_approve: false,
            sandbox: None,
            dangerously_bypass: false,
            process_id: (!terminal).then_some(42),
            created_at_ms: 1,
            updated_at_ms: 1,
            exit_code: None,
            error: None,
            title: None,
            parser: vt100::Parser::new_with_callbacks(
                INITIAL_ROWS,
                INITIAL_COLS,
                SCROLLBACK_ROWS,
                TitleCallbacks::default(),
            ),
            chunks: VecDeque::new(),
            transcript_bytes: 0,
            next_cursor: 0,
            last_output_at_ms: None,
            process_done: terminal,
            reader_done: terminal,
            hook: HookState::default(),
        }),
        changed: Condvar::new(),
        host_revision,
        writer_tx: Mutex::new(Some(writer_tx)),
        pending_writer_bytes: AtomicUsize::new(0),
        master: Mutex::new(None),
        #[cfg(unix)]
        termination: Mutex::new(TerminationLevel::default()),
        #[cfg(windows)]
        scope_job: None,
        close_lock: Mutex::new(()),
        shutdown: AtomicBool::new(false),
        cleanup_claimed: AtomicBool::new(false),
        cleanup_committed: AtomicBool::new(false),
    }
}

fn test_host(provider_session_id: &str, phase: SessionPhase) -> SessionHost {
    test_host_with_session(provider_session_id, phase).0
}

/// Build a host that owns one test session plus the session itself, so
/// tests can drive host APIs and manipulate the session (e.g. its close
/// lock) from outside the session module.
fn test_host_with_session(
    provider_session_id: &str,
    phase: SessionPhase,
) -> (SessionHost, Arc<Session>) {
    let revision = Arc::new(HostRevision::new());
    let session = Arc::new(test_session_with_revision(phase, Arc::clone(&revision)));
    let handle = session.state().unwrap().session;
    let host = SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions: HashMap::from([(handle.clone(), Arc::clone(&session))]),
            provider_sessions: HashMap::from([(provider_session_id.to_owned(), handle)]),
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    };
    (host, session)
}

pub(crate) fn with_test_host_holding_close_lock<R>(
    provider_session_id: &str,
    phase: SessionPhase,
    run: impl FnOnce(SessionHost) -> R,
) -> R {
    let (host, session) = test_host_with_session(provider_session_id, phase);
    let _held = session.test_hold_close_lock();
    run(host)
}

pub(crate) fn test_host_with_poisoned_close_lock(
    provider_session_id: &str,
    phase: SessionPhase,
) -> SessionHost {
    let (host, session) = test_host_with_session(provider_session_id, phase);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.test_poison_close_lock();
    }));
    assert!(poisoned.is_err());
    host
}

#[cfg(windows)]
fn windows_process_host(root_pid: u32, job: WindowsJob) -> SessionHost {
    let revision = Arc::new(HostRevision::new());
    let mut session = test_session_with_revision(SessionPhase::Running, Arc::clone(&revision));
    session.scope_id = root_pid;
    session.scope_job = Some(job);
    session.inner.get_mut().unwrap().process_id = Some(root_pid);
    let session = Arc::new(session);
    SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions: HashMap::from([("gbt-test".to_owned(), Arc::clone(&session))]),
            provider_sessions: HashMap::from([(
                TEST_PROVIDER_SESSION_ID.to_owned(),
                "gbt-test".to_owned(),
            )]),
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    }
}

/// Build a host that owns one Running test session whose immutable
/// `scope_id` addresses a real Unix process group. Mirrors
/// `windows_process_host`: the scope id is set before the session is
/// wrapped in an Arc and inserted into the registry.
#[cfg(unix)]
fn unix_process_host(pgid: u32) -> SessionHost {
    let revision = Arc::new(HostRevision::new());
    let mut session = test_session_with_revision(SessionPhase::Running, Arc::clone(&revision));
    session.scope_id = pgid;
    session.inner.get_mut().unwrap().process_id = Some(pgid);
    let session = Arc::new(session);
    SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions: HashMap::from([("gbt-test".to_owned(), Arc::clone(&session))]),
            provider_sessions: HashMap::from([(
                TEST_PROVIDER_SESSION_ID.to_owned(),
                "gbt-test".to_owned(),
            )]),
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    }
}

/// Like `unix_process_host` but registers one Running session per process
/// group, with handles `gbt-1`, `gbt-2`, ... in pgid order.
#[cfg(unix)]
fn unix_process_hosts(pgids: &[u32]) -> SessionHost {
    let revision = Arc::new(HostRevision::new());
    let mut sessions = HashMap::new();
    for (index, &pgid) in pgids.iter().enumerate() {
        let handle = format!("gbt-{}", index + 1);
        let mut session = test_session_with_revision(SessionPhase::Running, Arc::clone(&revision));
        session.inner.get_mut().unwrap().session = handle.clone();
        session.scope_id = pgid;
        session.inner.get_mut().unwrap().process_id = Some(pgid);
        sessions.insert(handle, Arc::new(session));
    }
    let provider_sessions = sessions
        .keys()
        .cloned()
        .map(|handle| (handle.clone(), handle))
        .collect();
    SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions,
            provider_sessions,
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    }
}

#[test]
fn resolves_grok_state_directory_with_platform_precedence() {
    assert_eq!(
        worker::worker_state_dir_from(
            Some(OsString::from("/custom/grok")),
            Some(OsString::from("/home/test")),
            Some(OsString::from(r"C:\Users\test")),
            false,
            ".grok",
        ),
        Some(PathBuf::from("/custom/grok"))
    );
    assert_eq!(
        worker::worker_state_dir_from(
            None,
            Some(OsString::from("/home/test")),
            Some(OsString::from(r"C:\Users\test")),
            false,
            ".grok",
        ),
        Some(PathBuf::from("/home/test").join(".grok"))
    );
    assert_eq!(
        worker::worker_state_dir_from(
            None,
            Some(OsString::from("/home/test")),
            Some(OsString::from(r"C:\Users\test")),
            true,
            ".grok",
        ),
        Some(PathBuf::from(r"C:\Users\test").join(".grok"))
    );
    assert_eq!(
        worker::worker_state_dir_from(Some(OsString::new()), None, None, false, ".grok"),
        None
    );
}

#[test]
fn probes_writable_grok_state_directory_and_removes_probe() {
    let root = temporary_test_directory("writable-state");
    let state_dir = root.join("state");
    worker::ensure_worker_state_dir_writable_at(
        WorkerKind::Grok,
        &state_dir,
        TEST_PROVIDER_SESSION_ID,
    )
    .unwrap();
    assert!(state_dir.is_dir());
    assert!(
        !state_dir
            .join(format!(
                ".agent-bridge-write-probe-{TEST_PROVIDER_SESSION_ID}"
            ))
            .exists()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reports_unwritable_grok_state_directory_with_sandbox_guidance() {
    let root = temporary_test_directory("blocked-state");
    fs::create_dir_all(&root).unwrap();
    let state_dir = root.join("state-file");
    fs::write(&state_dir, b"not a directory").unwrap();
    let error = worker::ensure_worker_state_dir_writable_at(
        WorkerKind::Grok,
        &state_dir,
        TEST_PROVIDER_SESSION_ID,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("Grok state directory is not writable"));
    assert!(error.contains("filesystem sandbox"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn probe_collision_preserves_existing_file() {
    let root = temporary_test_directory("probe-collision");
    fs::create_dir_all(&root).unwrap();
    let probe_path = root.join(format!(
        ".agent-bridge-write-probe-{TEST_PROVIDER_SESSION_ID}"
    ));
    fs::write(&probe_path, b"existing").unwrap();
    assert!(
        worker::ensure_worker_state_dir_writable_at(
            WorkerKind::Grok,
            &root,
            TEST_PROVIDER_SESSION_ID
        )
        .is_err()
    );
    assert_eq!(fs::read(&probe_path).unwrap(), b"existing");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn detects_grok_working_and_idle_titles() {
    assert_eq!(
        phase_from_title(WorkerKind::Grok, Some("⠋ - Waiting for response… - grok")),
        Some(SessionPhase::Running)
    );
    assert_eq!(
        phase_from_title(WorkerKind::Grok, Some("Fix the auth bug - grok")),
        Some(SessionPhase::Idle)
    );
    assert_eq!(
        phase_from_title(WorkerKind::Grok, Some("grok")),
        Some(SessionPhase::Idle)
    );
    assert_eq!(phase_from_title(WorkerKind::Grok, Some("PowerShell")), None);
}

#[test]
fn builds_only_interactive_grok_arguments() {
    let config = LaunchConfig {
        worker: WorkerKind::Grok,
        bin: OsString::from("grok.exe"),
        cwd: PathBuf::from(r"C:\repo"),
        prompt: Some("修复中文".to_owned()),
        model: Some("grok-4".to_owned()),
        owner: None,
        always_approve: true,
        sandbox: None,
        dangerously_bypass: false,
        client_kind: None,
        client_session_id: None,
        client_lease: None,
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
    };
    let grok_home = PathBuf::from(r"C:\repo\.grok-state");
    let command = worker::build_worker_command(
        config.worker,
        &config.bin,
        &config.cwd,
        config.prompt.as_deref(),
        config.model.as_deref(),
        config.always_approve,
        config.sandbox,
        config.dangerously_bypass,
        TEST_PROVIDER_SESSION_ID,
        Some(&grok_home),
    );
    assert_eq!(command.get_env("GROK_BRIDGE_SESSION"), None);
    assert_eq!(command.get_env("GROK_BRIDGE_HOOK_TOKEN"), None);
    assert_eq!(command.get_env("GROK_HOME"), Some(grok_home.as_os_str()));
    let argv = command
        .get_argv()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        argv,
        [
            "grok.exe",
            "--session-id",
            TEST_PROVIDER_SESSION_ID,
            "--always-approve",
            "--model",
            "grok-4",
            "修复中文"
        ]
    );
    assert!(!argv.iter().any(|value| value == "-p"));
    assert!(!argv.iter().any(|value| value == "--output-format"));
}

#[cfg(windows)]
#[test]
fn builds_gated_windows_launcher_before_the_worker_command() {
    let config = LaunchConfig {
        worker: WorkerKind::Grok,
        bin: OsString::from(r"C:\Program Files\Grok\grok.exe"),
        cwd: PathBuf::from(r"C:\repo"),
        prompt: Some("fix it".to_owned()),
        model: Some("grok-4".to_owned()),
        owner: None,
        always_approve: false,
        sandbox: None,
        dangerously_bypass: false,
        client_kind: None,
        client_session_id: None,
        client_lease: None,
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
    };
    let grok_home = PathBuf::from(r"C:\repo\.grok-state");
    let (command, gate, job) =
        build_windows_launcher_command(&config, TEST_PROVIDER_SESSION_ID, Some(&grok_home))
            .unwrap();
    let argv = command.get_argv();
    assert_eq!(PathBuf::from(&argv[0]), env::current_exe().unwrap());
    assert_eq!(argv[1], OsString::from("__windows-job-child"));
    assert_eq!(argv[2], gate.event_name);
    assert_eq!(argv[3], gate.pid_report_name);
    assert_eq!(argv[4], OsString::from(r"C:\Program Files\Grok\grok.exe"));
    assert_eq!(argv[5], OsString::from("--session-id"));
    assert_eq!(argv[6], OsString::from(TEST_PROVIDER_SESSION_ID));
    assert_eq!(
        command.get_cwd().map(OsString::as_os_str),
        Some(config.cwd.as_os_str())
    );
    assert_eq!(command.get_env("GROK_HOME"), Some(grok_home.as_os_str()));
    assert_eq!(job.process_scope_alive(), ScopeAlive::Gone);
}

#[cfg(windows)]
#[test]
fn launch_gate_reports_the_real_grok_pid_without_a_file_or_shell() {
    let gate = WindowsLaunchGate::create().unwrap();
    report_windows_worker_pid(&gate.pid_report_name, 1234).unwrap();
    assert_eq!(gate.wait_for_worker_pid(100, None).unwrap(), 1234);
}

#[cfg(windows)]
#[test]
fn unassigned_windows_launcher_gate_has_a_bounded_wait() {
    let gate = WindowsLaunchGate::create().unwrap();
    let error = wait_for_windows_launch_gate(&gate.event_name, 1).unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
}

#[cfg(windows)]
#[test]
fn launch_gate_signal_before_wait_is_not_lost() {
    let gate = WindowsLaunchGate::create().unwrap();
    gate.signal().unwrap();
    wait_for_windows_launch_gate(&gate.event_name, 100).unwrap();
}

#[test]
fn title_callbacks_reply_to_conpty_cursor_position_report() {
    let mut parser =
        vt100::Parser::new_with_callbacks(INITIAL_ROWS, INITIAL_COLS, 0, TitleCallbacks::default());
    parser.process(b"\x1b[6n");
    let responses = std::mem::take(&mut parser.callbacks_mut().responses);
    assert_eq!(responses, vec![b"\x1b[1;1R".to_vec()]);
    parser.process(b"\x1b[5n");
    let responses = std::mem::take(&mut parser.callbacks_mut().responses);
    assert_eq!(responses, vec![b"\x1b[0n".to_vec()]);
}

#[cfg(windows)]
#[test]
fn handshake_error_includes_escaped_pre_handshake_pty_output() {
    let session = test_session(SessionPhase::Starting);
    session.append_output(b"\x1b[6nhelper-failed".to_vec());
    let output = session.bounded_pre_handshake_output();
    assert!(output.contains("\\x1b[6n"), "{output}");
    assert!(output.contains("helper-failed"), "{output}");
}

#[cfg(windows)]
#[test]
fn windows_job_child_handshake_helper() {
    if env::var_os(WINDOWS_JOB_CHILD_HELPER_ENV).is_none() {
        return;
    }
    let gate_name = env::var_os(WINDOWS_JOB_CHILD_GATE_ENV)
        .expect("job-child helper launch gate must be provided");
    let pid_name = env::var_os(WINDOWS_JOB_CHILD_PID_ENV)
        .expect("job-child helper PID report must be provided");
    let status = run_windows_job_child(vec![
        gate_name,
        pid_name,
        OsString::from("cmd.exe"),
        OsString::from("/c"),
        OsString::from("exit 0"),
    ])
    .expect("job-child helper must complete");
    assert_eq!(status, 0);
}

#[cfg(windows)]
#[test]
fn conpty_job_child_handshake_reports_pid() {
    let gate = WindowsLaunchGate::create().unwrap();
    let job = WindowsJob::create().unwrap();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            cols: INITIAL_COLS,
            rows: INITIAL_ROWS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let mut command = CommandBuilder::new(env::current_exe().unwrap());
    command.args(["--exact", WINDOWS_JOB_CHILD_HELPER_TEST, "--nocapture"]);
    command.env(WINDOWS_JOB_CHILD_HELPER_ENV, "1");
    command.env(WINDOWS_JOB_CHILD_GATE_ENV, &gate.event_name);
    command.env(WINDOWS_JOB_CHILD_PID_ENV, &gate.pid_report_name);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    pump_conpty_replies(reader, writer);
    job.assign_process(child.as_raw_handle().expect("launcher handle"))
        .unwrap();
    gate.signal().unwrap();
    let started = Instant::now();
    let pid = gate
        .wait_for_worker_pid(5_000, child.as_raw_handle())
        .expect("ConPTY job-child handshake must report a PID");
    let _ = child.kill();
    assert_ne!(pid, 0);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "handshake took {:?}",
        started.elapsed()
    );
}

#[cfg(windows)]
#[test]
fn failed_handshake_terminates_the_job_scope() {
    let gate = WindowsLaunchGate::create().unwrap();
    let job = WindowsJob::create().unwrap();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            cols: INITIAL_COLS,
            rows: INITIAL_ROWS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let mut command = CommandBuilder::new(env::current_exe().unwrap());
    command.args(["--exact", WINDOWS_JOB_CHILD_HELPER_TEST, "--nocapture"]);
    command.env(WINDOWS_JOB_CHILD_HELPER_ENV, "1");
    command.env(WINDOWS_JOB_CHILD_GATE_ENV, &gate.event_name);
    command.env(WINDOWS_JOB_CHILD_PID_ENV, &gate.pid_report_name);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    pump_conpty_replies(reader, writer);
    job.assign_process(child.as_raw_handle().expect("launcher handle"))
        .unwrap();
    let error = gate
        .wait_for_worker_pid(200, child.as_raw_handle())
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    job.terminate(TerminationLevel::Kill).unwrap();
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if job.process_scope_alive() == ScopeAlive::Gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Job Object still had members after handshake failure cleanup"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn pump_conpty_replies(mut reader: Box<dyn Read + Send>, mut writer: Box<dyn Write + Send>) {
    thread::spawn(move || {
        let mut parser = vt100::Parser::new_with_callbacks(
            INITIAL_ROWS,
            INITIAL_COLS,
            0,
            TitleCallbacks::default(),
        );
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    parser.process(&buffer[..read]);
                    let responses = std::mem::take(&mut parser.callbacks_mut().responses);
                    for response in responses {
                        if writer
                            .write_all(&response)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(_) => return,
            }
        }
    });
}

#[cfg(windows)]
#[test]
fn wait_for_worker_pid_fails_fast_when_the_launcher_exits() {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Command, Stdio};

    let gate = WindowsLaunchGate::create().unwrap();
    let mut child = Command::new("cmd.exe")
        .args(["/c", "exit", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let error = gate
        .wait_for_worker_pid(10_000, Some(child.as_raw_handle()))
        .unwrap_err();
    let _ = child.wait();
    assert!(
        error.to_string().contains("exited with code 7"),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[cfg(windows)]
#[test]
fn wait_for_worker_pid_still_reports_when_watching_a_live_launcher() {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Command, Stdio};

    let gate = WindowsLaunchGate::create().unwrap();
    let mut child = Command::new("cmd.exe")
        .args(["/c", "ping", "-n", "8", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    report_windows_worker_pid(&gate.pid_report_name, 1234).unwrap();
    let pid = gate.wait_for_worker_pid(1_000, Some(child.as_raw_handle()));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(pid.unwrap(), 1234);
}

#[test]
fn publishes_terminal_phase_only_after_process_and_reader_finish() {
    assert_eq!(
        completed_phase(SessionPhase::Running, true, false, false, false, Some(0)),
        None
    );
    assert_eq!(
        completed_phase(SessionPhase::Running, true, true, false, false, Some(0)),
        Some(SessionPhase::Exited)
    );
    assert_eq!(
        completed_phase(SessionPhase::Running, true, true, true, false, Some(1)),
        Some(SessionPhase::Stopped)
    );
    // An explicit close finalizes as soon as the process scope is verified
    // gone; a missing reader EOF must not keep the record half-open.
    assert_eq!(
        completed_phase(SessionPhase::Running, true, false, true, false, None),
        Some(SessionPhase::Stopped)
    );
    // Without an explicit close, reader EOF is still required.
    assert_eq!(
        completed_phase(SessionPhase::Running, true, false, false, true, None),
        None
    );
}

#[test]
fn generates_random_uuid_v4_provider_session_ids() {
    let first = generate_provider_session_id().unwrap();
    let second = generate_provider_session_id().unwrap();
    assert_eq!(first.len(), 36);
    assert_eq!(&first[8..9], "-");
    assert_eq!(&first[13..14], "-");
    assert_eq!(&first[18..19], "-");
    assert_eq!(&first[23..24], "-");
    assert_eq!(&first[14..15], "4");
    assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
    assert!(
        first
            .bytes()
            .all(|byte| { byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'-') })
    );
    assert_ne!(first, second);
}

#[test]
fn maps_hook_lifecycle_and_tool_events_to_activity() {
    let mut event = hook_event(HookEventKind::PreToolUse);
    event.tool_name = Some("read_file".to_owned());
    assert_eq!(
        hook_effect(&event),
        HookEffect::Working {
            tool_name: Some("read_file".to_owned())
        }
    );

    event.tool_name = Some("ASK_USER_QUESTION".to_owned());
    event.message = Some("请选择目标".to_owned());
    assert_eq!(
        hook_effect(&event),
        HookEffect::Waiting {
            tool_name: Some("ASK_USER_QUESTION".to_owned()),
            reason: "请选择目标".to_owned(),
        }
    );

    for kind in [
        HookEventKind::UserPromptSubmit,
        HookEventKind::PostToolUse,
        HookEventKind::PostToolUseFailure,
        HookEventKind::PermissionDenied,
        HookEventKind::PreCompact,
        HookEventKind::PostCompact,
    ] {
        assert!(matches!(
            hook_effect(&hook_event(kind)),
            HookEffect::Working { .. }
        ));
    }
    for kind in [
        HookEventKind::Stop,
        HookEventKind::StopFailure,
        HookEventKind::SessionEnd,
    ] {
        assert_eq!(hook_effect(&hook_event(kind)), HookEffect::Done);
    }
    assert_eq!(
        hook_effect(&hook_event(HookEventKind::SessionStart)),
        HookEffect::Reset
    );
    for kind in [HookEventKind::SubagentStart, HookEventKind::SubagentStop] {
        assert_eq!(hook_effect(&hook_event(kind)), HookEffect::RecordOnly);
    }
}

#[test]
fn completed_turn_ignores_late_tool_events_until_the_next_prompt() {
    let session = test_session(SessionPhase::Running);
    session
        .apply_hook_event(hook_event(HookEventKind::Stop))
        .unwrap();
    let stopped = session.state().unwrap();
    assert_eq!(stopped.phase, SessionPhase::Idle);
    assert_eq!(stopped.activity, HookActivity::Done);

    let mut late = hook_event(HookEventKind::PostToolUse);
    late.tool_name = Some("edit_file".to_owned());
    session.apply_hook_event(late).unwrap();
    let guarded = session.state().unwrap();
    assert_eq!(guarded.phase, SessionPhase::Idle);
    assert_eq!(guarded.activity, HookActivity::Done);
    assert_eq!(guarded.tool_name, None);

    session
        .apply_hook_event(hook_event(HookEventKind::UserPromptSubmit))
        .unwrap();
    let resumed = session.state().unwrap();
    assert_eq!(resumed.phase, SessionPhase::Running);
    assert_eq!(resumed.activity, HookActivity::Working);
}

#[test]
fn lease_cleanup_only_targets_idle_or_terminal_sessions_after_grace() {
    let session = test_session(SessionPhase::Idle);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-thread".to_owned());
        inner.client_lease = Some(lease);
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 100,
            grace_ms: 200,
        };
        inner.phase_changed_at_ms = 900;
        assert_eq!(
            inner.client_lifecycle(1_050, false).0,
            ClientLeaseState::Connected
        );
        let lifecycle = inner.client_lifecycle(1_101, false);
        assert_eq!(lifecycle.0, ClientLeaseState::Orphaned);
        assert_eq!(lifecycle.2, Some(1_100));
        assert_eq!(lifecycle.3, Some(1_300));
        assert!(!inner.orphan_cleanup_due(1_299));
        assert!(inner.orphan_cleanup_due(1_300));

        set_phase(&mut inner, SessionPhase::Running, 1_200);
        let running = inner.client_lifecycle(2_000, false);
        assert_eq!(running.0, ClientLeaseState::Disconnected);
        assert_eq!(running.3, None);
        assert!(!inner.orphan_cleanup_due(10_000));
    }
}

#[test]
fn web_keepalive_refreshes_leases_and_cancels_pending_cleanup() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Idle);
    let session = host.get("gbt-test").unwrap();
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-web".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 100,
            grace_ms: 200,
        };
        inner.phase_changed_at_ms = 900;
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-web".to_owned(), Arc::clone(&lease));

    assert!(session.claim_orphan_cleanup(1_300).unwrap());
    assert!(session.cleanup_claimed.load(Ordering::Acquire));
    let before = host.revision();
    assert_eq!(host.touch_web_clients_at(1_350).unwrap(), 1);
    assert_eq!(lease.load(Ordering::Acquire), 1_350);
    assert!(!session.cleanup_claimed.load(Ordering::Acquire));
    assert!(!session.cleanup_committed.load(Ordering::Acquire));
    assert_ne!(host.revision(), before);
    assert_eq!(
        session
            .inner
            .lock()
            .unwrap()
            .client_lifecycle(1_400, false)
            .0,
        ClientLeaseState::Connected
    );
}

#[test]
fn final_orphan_commit_rechecks_lease_and_blocks_late_input() {
    let session = test_session(SessionPhase::Idle);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-race".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 100,
            grace_ms: 200,
        };
        inner.phase_changed_at_ms = 900;
    }

    assert!(session.claim_orphan_cleanup(1_300).unwrap());
    lease.store(1_300, Ordering::Release);
    assert!(!session.commit_orphan_cleanup(1_300).unwrap());
    assert!(!session.cleanup_claimed.load(Ordering::Acquire));

    assert!(session.claim_orphan_cleanup(1_600).unwrap());
    assert!(session.commit_orphan_cleanup(1_600).unwrap());
    let error = session.write_raw(b"new task\r".to_vec()).unwrap_err();
    assert!(format!("{error:#}").contains("session cleanup has started"));
}

#[test]
fn client_heartbeat_cancels_claim_before_input_is_accepted() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Idle);
    let session = host.get("gbt-test").unwrap();
    let lease = Arc::new(AtomicU64::new(1_000));
    let (writer_tx, writer_rx) = sync_channel(1);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);
    {
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-resume".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 100,
            grace_ms: 200,
        };
        inner.phase_changed_at_ms = 900;
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-resume".to_owned(), lease);

    assert!(session.claim_orphan_cleanup(1_300).unwrap());
    host.touch_client_at("codex-resume", 1_300).unwrap();
    assert!(!session.cleanup_claimed.load(Ordering::Acquire));
    session.write_raw(b"resume\r".to_vec()).unwrap();
    let item = writer_rx.recv().unwrap();
    assert_eq!(item.data, b"resume\r");
    assert_eq!(item.budget, b"resume\r".len());
    assert_eq!(session.state().unwrap().phase, SessionPhase::Running);
}

#[test]
fn writer_admission_rejects_whole_input_once_byte_budget_is_full() {
    let session = test_session(SessionPhase::Idle);
    let (writer_tx, writer_rx) = sync_channel(64);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    let payload = vec![0x61; crate::protocol::MAX_WRITE_BYTES];
    for _ in 0..4 {
        session.write_raw(payload.clone()).unwrap();
    }
    assert_eq!(
        session.pending_writer_bytes.load(Ordering::Acquire),
        4 * crate::protocol::MAX_WRITE_BYTES
    );

    // A fifth maximum-size write would exceed the byte budget: the whole
    // input is rejected and no bytes enter the queue.
    let error = session.write_raw(payload.clone()).unwrap_err();
    assert!(format!("{error:#}").contains("queue is full"));
    assert_eq!(
        session.pending_writer_bytes.load(Ordering::Acquire),
        4 * crate::protocol::MAX_WRITE_BYTES
    );
    // Exactly the four accepted writes are queued; nothing partial.
    let queued = drain_writer(&writer_rx);
    assert_eq!(queued.len(), 4);
    assert!(queued.iter().all(|item| item.data == payload));
    assert!(
        queued
            .iter()
            .all(|item| item.budget == crate::protocol::MAX_WRITE_BYTES)
    );
    // Entry-count admission is still enforced for small payloads.
    let (tiny_tx, tiny_rx) = sync_channel(1);
    *session.writer_tx.lock().unwrap() = Some(tiny_tx);
    session.pending_writer_bytes.store(0, Ordering::Release);
    assert!(session.write_raw(b"a".to_vec()).is_ok());
    assert!(
        format!("{:#}", session.write_raw(b"b".to_vec()).unwrap_err()).contains("queue is full")
    );
    drop(tiny_rx);
}

#[test]
fn writer_byte_budget_is_released_after_bytes_are_written() {
    let session = Arc::new(test_session(SessionPhase::Idle));
    let (writer_tx, writer_rx) = sync_channel(8);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    spawn_writer(Arc::clone(&session), Box::new(std::io::sink()), writer_rx);
    session.write_raw(vec![0x62; 4096]).unwrap();
    session.write_raw(vec![0x63; 8192]).unwrap();
    // The writer thread drains concurrently; the budget must never exceed
    // the sum and must settle back to zero once everything is written.
    let initial = session.pending_writer_bytes.load(Ordering::Acquire);
    assert!(initial <= 4096 + 8192);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while session.pending_writer_bytes.load(Ordering::Acquire) != 0 {
        assert!(std::time::Instant::now() < deadline, "writer never drained");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Channel closed: the writer loop exits; budget stays released.
    session.close_writer();
}

#[test]
fn terminal_responses_never_count_toward_the_writer_byte_budget() {
    let session = test_session(SessionPhase::Idle);
    let (writer_tx, writer_rx) = sync_channel(64);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    // Interleave the two production enqueue paths: write_raw admits bytes,
    // queue_terminal_response (vt100 response frames) must not touch the
    // admission counter at all.
    session.write_raw(vec![0x62; 2048]).unwrap();
    session.queue_terminal_response(b"\x1b[0n".to_vec());
    session.write_raw(vec![0x63; 4096]).unwrap();
    session.queue_terminal_response(b"\x1b[?1;2c".to_vec());
    session.write_raw(b"tail".to_vec()).unwrap();

    // Budget reflects only the admitted input bytes, never the responses.
    assert_eq!(
        session.pending_writer_bytes.load(Ordering::Acquire),
        2048 + 4096 + b"tail".len()
    );

    let items = drain_writer(&writer_rx);
    assert_eq!(
        items.iter().map(|item| item.budget).collect::<Vec<_>>(),
        vec![2048, 0, 4096, 0, b"tail".len()]
    );
    // Interleaved response frames still reach the PTY in arrival order.
    assert_eq!(&items[1].data, b"\x1b[0n");
    assert_eq!(&items[3].data, b"\x1b[?1;2c");
    // Budget stays unchanged once the queue is drained.
    assert_eq!(
        session.pending_writer_bytes.load(Ordering::Acquire),
        2048 + 4096 + b"tail".len()
    );
}

#[test]
fn writer_mixed_input_and_terminal_response_drain_settles_budget_to_zero() {
    let session = Arc::new(test_session(SessionPhase::Idle));
    let (writer_tx, writer_rx) = sync_channel(64);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    spawn_writer(Arc::clone(&session), Box::new(std::io::sink()), writer_rx);
    session.write_raw(vec![0x62; 4096]).unwrap();
    session.queue_terminal_response(b"\x1b[0n".to_vec());
    session.write_raw(vec![0x63; 8192]).unwrap();
    session.queue_terminal_response(b"\x1b[?1;2c".to_vec());

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while session.pending_writer_bytes.load(Ordering::Acquire) != 0 {
        assert!(std::time::Instant::now() < deadline, "writer never drained");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Responses contributed zero budget, so the counter can only settle to
    // exactly zero when releases never exceeded adds: no underflow.
    assert_eq!(session.pending_writer_bytes.load(Ordering::Acquire), 0);
    session.close_writer();
}

fn drain_writer(rx: &std::sync::mpsc::Receiver<WriterItem>) -> Vec<WriterItem> {
    let mut out = Vec::new();
    while let Ok(item) = rx.try_recv() {
        out.push(item);
    }
    out
}

#[test]
fn orphan_reaper_removes_expired_terminal_sessions_but_keeps_running_ones() {
    let expired = Arc::new(AtomicU64::new(1));
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Exited);
    {
        let session = host.get("gbt-test").unwrap();
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-expired".to_owned());
        inner.client_lease = Some(Arc::clone(&expired));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 1,
            grace_ms: 1,
        };
        inner.phase_changed_at_ms = 1;
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-expired".to_owned(), expired);
    let result = host.reap_orphans().unwrap();
    assert_eq!(result.matched, 1);
    assert_eq!(result.closed, 1);
    assert!(result.failures.is_empty());
    assert!(host.list().unwrap().is_empty());

    let running = Arc::new(AtomicU64::new(1));
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    {
        let session = host.get("gbt-test").unwrap();
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-running".to_owned());
        inner.client_lease = Some(Arc::clone(&running));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 1,
            grace_ms: 1,
        };
        inner.phase_changed_at_ms = 1;
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-running".to_owned(), running);
    let result = host.reap_orphans().unwrap();
    assert_eq!(result.matched, 0);
    assert_eq!(host.list().unwrap().len(), 1);
    assert_eq!(
        host.show("gbt-test").unwrap().client_state,
        ClientLeaseState::Disconnected
    );
}

#[test]
fn classifies_notification_events_without_treating_permission_prompt_as_blocked() {
    let mut event = hook_event(HookEventKind::Notification);
    event.notification_type = Some("permission_prompt".to_owned());
    event.message = Some("Approval required".to_owned());
    assert_eq!(hook_effect(&event), HookEffect::RecordOnly);

    event.notification_type = Some("question".to_owned());
    event.message = Some("请选择".to_owned());
    assert_eq!(
        hook_effect(&event),
        HookEffect::Waiting {
            tool_name: None,
            reason: "请选择".to_owned(),
        }
    );

    event.notification_type = Some("input_required".to_owned());
    event.message = None;
    assert_eq!(hook_effect(&event), HookEffect::Done);

    event.notification_type = Some("status".to_owned());
    event.level = Some("info".to_owned());
    assert_eq!(hook_effect(&event), HookEffect::RecordOnly);
}

#[test]
fn applies_hook_state_without_advancing_the_read_cursor() {
    let session = test_session(SessionPhase::Running);
    let cwd = session.state().unwrap().cwd;
    let mut event = hook_event(HookEventKind::PreToolUse);
    event.cwd = Some(cwd);
    event.tool_name = Some("ask_user_question".to_owned());
    event.message = Some("需要选择".to_owned());
    session.apply_hook_event(event).unwrap();

    let web = session.state().unwrap();
    assert_eq!(web.phase, SessionPhase::Running);
    assert_eq!(web.activity, HookActivity::Waiting);
    assert_eq!(web.tool_name.as_deref(), Some("ask_user_question"));
    assert_eq!(web.waiting_reason.as_deref(), Some("需要选择"));
    assert_eq!(web.last_cursor, 0);
    let read = session.read(0, 1, 0).unwrap();
    assert_eq!(read.cursor, 0);
    assert_eq!(read.next_cursor, 0);
    let wait = session.wait(WaitCondition::TuiIdle, 0).unwrap();
    assert!(!wait.satisfied);
    assert!(!wait.timed_out);
    assert_eq!(wait.blocked_reason.as_deref(), Some("需要选择"));

    let serialized = serde_json::to_value(web).unwrap();
    assert_eq!(serialized["session"], "gbt-test");
    assert_eq!(serialized["activity"], "waiting");
    assert!(serialized.get("hook_token").is_none());
    assert!(serialized.get("provider_session_id").is_none());
}

#[test]
fn ignores_late_terminal_hook_events() {
    let terminal = test_session(SessionPhase::Exited);
    let mut late = hook_event(HookEventKind::PreToolUse);
    late.cwd = Some("path-that-does-not-exist".to_owned());
    late.tool_name = Some("ask_user_question".to_owned());
    terminal.apply_hook_event(late).unwrap();
    let web = terminal.state().unwrap();
    assert_eq!(web.phase, SessionPhase::Exited);
    assert_eq!(web.activity, HookActivity::Unknown);
    assert_eq!(web.hook_event, None);
}

#[test]
fn routes_hook_events_by_provider_session_id() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let cwd = host.show("gbt-test").unwrap().cwd;
    let mut event = hook_event(HookEventKind::PreToolUse);
    event.cwd = Some(cwd);
    event.tool_name = Some("ask_user_question".to_owned());
    event.message = Some("需要选择".to_owned());

    assert!(
        host.apply_hook_event(WorkerKind::Grok, TEST_PROVIDER_SESSION_ID, event)
            .unwrap()
    );
    let web = host.list_web().unwrap().pop().unwrap();
    assert_eq!(web.activity, HookActivity::Waiting);
    assert_eq!(web.tool_name.as_deref(), Some("ask_user_question"));
    assert_eq!(web.waiting_reason.as_deref(), Some("需要选择"));
}

#[test]
fn returns_false_for_unknown_provider_sessions() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    assert!(
        !host
            .apply_hook_event(
                WorkerKind::Grok,
                "00000000-0000-4000-8000-000000000000",
                hook_event(HookEventKind::Stop)
            )
            .unwrap()
    );
    assert_eq!(host.show("gbt-test").unwrap().phase, SessionPhase::Running);
}

#[test]
fn close_removes_the_provider_session_index() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Exited);
    assert!(host.close("gbt-test").unwrap());
    assert!(
        !host
            .apply_hook_event(
                WorkerKind::Grok,
                TEST_PROVIDER_SESSION_ID,
                hook_event(HookEventKind::Stop)
            )
            .unwrap()
    );
    let registry = host.registry.lock().unwrap();
    assert!(registry.sessions.is_empty());
    assert!(registry.provider_sessions.is_empty());
}

#[test]
fn close_client_removes_the_lease_and_publishes_a_revision() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Exited);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let session = host.get("gbt-test").unwrap();
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-close".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-close".to_owned(), lease);
    let seen = host.revision();
    let result = host.close_client("codex-close").unwrap();
    assert_eq!(result.closed, 1);
    assert!(
        host.registry.lock().unwrap().clients.is_empty(),
        "the client lease must be removed after a fully successful close"
    );
    // A WebUI waiter must observe a revision published after the lease
    // removal, not only the one apply_close_outcomes emitted before it.
    let advanced = host.wait_revision(seen, Duration::from_millis(50));
    // finalize_now, apply_close_outcomes, then the post-removal notification.
    assert_eq!(advanced, seen + 3);
    assert_eq!(host.revision(), seen + 3);
}

#[test]
fn close_client_preserves_a_session_created_during_close() {
    let (host, session) = test_host_with_session(TEST_PROVIDER_SESSION_ID, SessionPhase::Exited);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-race".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-race".to_owned(), Arc::clone(&lease));

    thread::scope(|scope| {
        let held = session.test_hold_close_lock();
        let references = Arc::strong_count(&session);
        let closing = scope.spawn(|| host.close_client("codex-race").unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&session) == references && Instant::now() < deadline {
            thread::yield_now();
        }
        let snapshot_taken = Arc::strong_count(&session) > references;
        let remaining = Arc::new(test_session_with_revision(
            SessionPhase::Exited,
            Arc::clone(&host.revision),
        ));
        {
            let mut inner = remaining.inner.lock().unwrap();
            inner.session = "gbt-new".to_owned();
            inner.client_session_id = Some("codex-race".to_owned());
            inner.client_lease = Some(Arc::clone(&lease));
        }
        host.registry
            .lock()
            .unwrap()
            .sessions
            .insert("gbt-new".to_owned(), remaining);
        drop(held);
        let result = closing.join().unwrap();
        assert!(
            snapshot_taken,
            "close must take its snapshot before the new session is inserted"
        );
        assert_eq!(result.matched, 1);
        assert_eq!(result.closed, 1);
    });
    {
        let registry = host.registry.lock().unwrap();
        assert!(registry.sessions.contains_key("gbt-new"));
        assert!(Arc::ptr_eq(
            registry.clients.get("codex-race").unwrap(),
            &lease
        ));
    }
    assert_eq!(host.close_client("codex-race").unwrap().closed, 1);
    assert!(
        !host
            .registry
            .lock()
            .unwrap()
            .clients
            .contains_key("codex-race")
    );
}

#[test]
fn reap_orphans_continues_after_claim_and_commit_lock_errors() {
    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 1,
        grace_ms: 1,
    });
    {
        let mut registry = host.registry.lock().unwrap();
        for handle in ["orphan-a", "orphan-b", "orphan-c"] {
            let session = Arc::new(test_session(SessionPhase::Exited));
            {
                let mut inner = session.inner.lock().unwrap();
                inner.session = handle.to_owned();
                inner.client_lease = Some(Arc::new(AtomicU64::new(1)));
                inner.orphan_policy = host.orphan_policy;
            }
            registry.sessions.insert(handle.to_owned(), session);
        }
    }
    // Freeze the map's iteration order: block the second claim while poisoning
    // the already-claimed first session, and poison the third before claiming.
    let ordered = host
        .registry
        .lock()
        .unwrap()
        .sessions
        .values()
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = ordered[2].inner.lock().unwrap();
            panic!("poison the claim candidate");
        }))
        .is_err()
    );
    let result = thread::scope(|scope| {
        let held = ordered[1].inner.lock().unwrap();
        let reaping = scope.spawn(|| host.reap_orphans().unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ordered[0].cleanup_claimed.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        let claimed = ordered[0].cleanup_claimed.load(Ordering::Acquire);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _inner = ordered[0].inner.lock().unwrap();
                panic!("poison the commit candidate");
            }))
            .is_err()
        );
        drop(held);
        let result = reaping.join().unwrap();
        assert!(
            claimed,
            "the first candidate must be claimed before poisoning"
        );
        result
    });
    assert_eq!(result.matched, 1);
    assert_eq!(result.closed, 1);
    assert_eq!(result.failures.len(), 2);
    assert!(
        result
            .failures
            .iter()
            .all(|message| message.contains("session state lock was poisoned"))
    );
    assert!(!ordered[0].cleanup_claimed.load(Ordering::Acquire));
    assert!(!ordered[0].cleanup_committed.load(Ordering::Acquire));
    assert!(!ordered[2].cleanup_claimed.load(Ordering::Acquire));
    assert!(!ordered[2].cleanup_committed.load(Ordering::Acquire));
    let registry = host.registry.lock().unwrap();
    assert_eq!(registry.sessions.len(), 2);
    assert!(
        !registry
            .sessions
            .values()
            .any(|session| Arc::ptr_eq(session, &ordered[1]))
    );
}

#[test]
fn close_client_keeps_the_lease_when_the_close_fails() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let session = host.get("gbt-test").unwrap();
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-fail".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
    }
    host.registry
        .lock()
        .unwrap()
        .clients
        .insert("codex-fail".to_owned(), lease);
    // Poison the close lock so the group close fails fast and
    // deterministically instead of terminating a real process scope.
    let session = host.get("gbt-test").unwrap();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.test_poison_close_lock();
    }));
    assert!(poisoned.is_err());
    let result = host.close_client("codex-fail").unwrap();
    assert_eq!(result.closed, 0);
    assert_eq!(result.failures.len(), 1);
    assert!(
        host.registry
            .lock()
            .unwrap()
            .clients
            .contains_key("codex-fail"),
        "a failed close must retain the client lease for a later retry"
    );
}

#[test]
fn hook_done_and_waiting_survive_output_without_an_explicit_grok_title() {
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Running,
            None,
            false,
            HookActivity::Done,
            false,
            false,
            false,
        ),
        SessionPhase::Idle
    );
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Running,
            None,
            false,
            HookActivity::Waiting,
            false,
            false,
            false,
        ),
        SessionPhase::Running
    );
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Idle,
            Some("⠋ - Waiting for response… - grok"),
            true,
            HookActivity::Done,
            false,
            false,
            false,
        ),
        SessionPhase::Running
    );
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Running,
            Some("grok"),
            true,
            HookActivity::Waiting,
            false,
            false,
            false,
        ),
        SessionPhase::Idle
    );
}

#[test]
fn quiet_fallback_recovers_from_a_missing_completion_hook() {
    let session = test_session(SessionPhase::Running);
    let mut inner = session.inner.lock().unwrap();
    inner.updated_at_ms = now_millis().saturating_sub(QUIET_IDLE_MILLISECONDS + 1);
    inner.hook.activity = HookActivity::Working;
    assert!(wait_satisfied(&mut inner, WaitCondition::TuiIdle));
    assert_eq!(inner.phase, SessionPhase::Idle);

    inner.phase = SessionPhase::Running;
    inner.updated_at_ms = now_millis().saturating_sub(QUIET_IDLE_MILLISECONDS + 1);
    inner.hook.activity = HookActivity::Waiting;
    assert!(!wait_satisfied(&mut inner, WaitCondition::TuiIdle));
    assert_eq!(inner.phase, SessionPhase::Running);
}

#[test]
fn idle_phase_projects_stale_working_activity_as_done() {
    let session = test_session(SessionPhase::Running);
    let mut inner = session.inner.lock().unwrap();
    inner.hook.activity = HookActivity::Working;

    inner.phase = SessionPhase::Running;
    assert_eq!(inner.to_state(1, false).activity, HookActivity::Working);
    inner.phase = SessionPhase::Idle;
    assert_eq!(inner.to_state(1, false).activity, HookActivity::Done);

    inner.phase = SessionPhase::Starting;
    assert_eq!(inner.to_state(1, false).activity, HookActivity::Working);
}

#[test]
fn terminal_phases_override_a_stale_hook_activity() {
    let session = test_session(SessionPhase::Running);
    let mut inner = session.inner.lock().unwrap();
    inner.hook.activity = HookActivity::Working;
    for phase in [
        SessionPhase::Failed,
        SessionPhase::Exited,
        SessionPhase::Stopped,
    ] {
        inner.phase = phase;
        assert_eq!(inner.to_state(1, false).activity, HookActivity::Unknown);
    }
    inner.hook.activity = HookActivity::Waiting;
    assert_eq!(inner.to_state(1, false).activity, HookActivity::Unknown);
    inner.hook.activity = HookActivity::Done;
    assert_eq!(inner.to_state(1, false).activity, HookActivity::Done);
}

#[test]
fn ctrl_c_send_preserves_bytes_and_does_not_start_or_finish_a_turn() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    let (writer_tx, writer_rx) = sync_channel(1);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    session
        .apply_hook_event(hook_event(HookEventKind::PreToolUse))
        .unwrap();
    session.send("\u{3}".to_owned()).unwrap();
    assert_eq!(writer_rx.recv().unwrap().data, b"\x03");
    let state = session.state().unwrap();
    assert_eq!(state.phase, SessionPhase::Running);
    assert_eq!(state.activity, HookActivity::Working);

    let idle = test_session(SessionPhase::Idle);
    let (idle_tx, idle_rx) = sync_channel(1);
    *idle.writer_tx.lock().unwrap() = Some(idle_tx);
    idle.send("\u{3}".to_owned()).unwrap();
    assert_eq!(idle_rx.recv().unwrap().data, b"\x03");
    let state = idle.state().unwrap();
    assert_eq!(state.phase, SessionPhase::Idle);
    assert_eq!(state.activity, HookActivity::Unknown);

    session.send("next task".to_owned()).unwrap();
    assert_eq!(
        writer_rx.recv().unwrap().data,
        b"\x1b[200~next task\x1b[201~\r"
    );

    let full = test_session(SessionPhase::Idle);
    let (full_tx, _full_rx) = sync_channel(1);
    full_tx
        .try_send(WriterItem {
            data: vec![b'x'],
            budget: 1,
        })
        .unwrap();
    *full.writer_tx.lock().unwrap() = Some(full_tx);
    let before = full.state().unwrap();
    let error = full.send("\u{3}".to_owned()).unwrap_err();
    assert!(error.to_string().contains("input queue is full"));
    let after = full.state().unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.activity, before.activity);

    let closed = test_session(SessionPhase::Idle);
    let (closed_tx, closed_rx) = sync_channel(1);
    drop(closed_rx);
    *closed.writer_tx.lock().unwrap() = Some(closed_tx);
    let before = closed.state().unwrap();
    let error = closed.send("\u{3}".to_owned()).unwrap_err();
    assert!(error.to_string().contains("input channel is closed"));
    let after = closed.state().unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.activity, before.activity);
}

#[test]
fn raw_write_passes_ctrl_c_through_without_starting_a_turn() {
    let session = test_session(SessionPhase::Idle);
    let (writer_tx, writer_rx) = sync_channel(1);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    session.write_raw(b"\x03".to_vec()).unwrap();
    assert_eq!(writer_rx.recv().unwrap().data, b"\x03");
    let state = session.state().unwrap();
    assert_eq!(state.phase, SessionPhase::Idle);
    assert_eq!(state.activity, HookActivity::Unknown);

    assert!(raw_input_starts_turn(b"\r"));
    assert!(raw_input_starts_turn(b"\n"));
    assert!(raw_input_starts_turn(b"new task\r"));
    assert!(!raw_input_starts_turn(b"\x03"));

    let full = test_session(SessionPhase::Idle);
    let (full_tx, _full_rx) = sync_channel(1);
    full_tx
        .try_send(WriterItem {
            data: vec![b'x'],
            budget: 1,
        })
        .unwrap();
    *full.writer_tx.lock().unwrap() = Some(full_tx);
    let before = full.state().unwrap();
    let error = full.write_raw(b"\x03".to_vec()).unwrap_err();
    assert!(error.to_string().contains("input queue is full"));
    let after = full.state().unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.activity, before.activity);

    let closed = test_session(SessionPhase::Idle);
    let (closed_tx, closed_rx) = sync_channel(1);
    drop(closed_rx);
    *closed.writer_tx.lock().unwrap() = Some(closed_tx);
    let before = closed.state().unwrap();
    let error = closed.write_raw(b"\x03".to_vec()).unwrap_err();
    assert!(error.to_string().contains("input channel is closed"));
    let after = closed.state().unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.activity, before.activity);
}

#[test]
fn late_output_does_not_revive_a_finished_process() {
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Exited,
            Some("grok"),
            true,
            HookActivity::Done,
            true,
            false,
            false,
        ),
        SessionPhase::Exited
    );
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Running,
            Some("grok"),
            false,
            HookActivity::Unknown,
            false,
            false,
            false
        ),
        SessionPhase::Running
    );
    assert_eq!(
        phase_after_output(
            WorkerKind::Grok,
            SessionPhase::Running,
            Some("grok"),
            true,
            HookActivity::Unknown,
            false,
            false,
            false
        ),
        SessionPhase::Idle
    );
}

#[cfg(windows)]
#[test]
fn normalizes_windows_verbatim_paths_for_child_processes() {
    assert_eq!(
        normalize_platform_path(PathBuf::from(r"\\?\D:\repo\project")),
        PathBuf::from(r"D:\repo\project")
    );
    assert_eq!(
        normalize_platform_path(PathBuf::from(r"\\?\UNC\server\share\repo")),
        PathBuf::from(r"\\server\share\repo")
    );
}

#[test]
fn detects_interactive_grok_and_codex_prompts_as_blocked() {
    assert_eq!(
        blocked_reason(WorkerKind::Grok, "Run Grok Build in a project directory?"),
        Some("grok-project-directory")
    );
    assert_eq!(
        blocked_reason(WorkerKind::Grok, "Type your answer here  Enter:submit"),
        Some("grok-interactive-prompt")
    );
    assert_eq!(blocked_reason(WorkerKind::Grok, "中文通讯正常"), None);
    assert_eq!(
        blocked_reason(
            WorkerKind::Codex,
            "Do you trust the contents of this directory?"
        ),
        Some("codex-trust-directory")
    );
}

#[test]
fn codex_session_start_hook_binds_external_provider_session_by_cwd() {
    let revision = Arc::new(HostRevision::new());
    let mut session = test_session_with_revision(SessionPhase::Starting, Arc::clone(&revision));
    session.inner.get_mut().unwrap().worker = WorkerKind::Codex;
    let session_cwd = session.inner.get_mut().unwrap().cwd.clone();
    let session = Arc::new(session);
    let host = SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions: HashMap::from([("gbt-codex".to_owned(), Arc::clone(&session))]),
            provider_sessions: HashMap::new(),
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    };

    let mut start = hook_event(HookEventKind::SessionStart);
    start.cwd = Some(session_cwd.clone());
    assert!(
        host.apply_hook_event(WorkerKind::Codex, TEST_PROVIDER_SESSION_ID, start)
            .unwrap()
    );
    // Grok hook event must NOT match the bound Codex session.
    assert!(
        !host
            .apply_hook_event(
                WorkerKind::Grok,
                TEST_PROVIDER_SESSION_ID,
                hook_event(HookEventKind::PreToolUse),
            )
            .unwrap()
    );
    // Subsequent Codex PreToolUse without cwd matches via bound provider_sessions.
    assert!(
        host.apply_hook_event(
            WorkerKind::Codex,
            TEST_PROVIDER_SESSION_ID,
            hook_event(HookEventKind::PreToolUse),
        )
        .unwrap()
    );
    assert_eq!(session.state().unwrap().phase, SessionPhase::Running);
}

#[test]
fn codex_non_session_start_stop_hook_binds_unbound_session_by_cwd() {
    let revision = Arc::new(HostRevision::new());
    let mut session = test_session_with_revision(SessionPhase::Running, Arc::clone(&revision));
    session.inner.get_mut().unwrap().worker = WorkerKind::Codex;
    let session_cwd = session.inner.get_mut().unwrap().cwd.clone();
    let session = Arc::new(session);
    let host = SessionHost {
        registry: Mutex::new(SessionRegistry {
            accepting: true,
            sessions: HashMap::from([("gbt-codex-stop".to_owned(), Arc::clone(&session))]),
            provider_sessions: HashMap::new(),
            external_bound_handles: HashSet::new(),
            clients: HashMap::new(),
            closed: HashMap::new(),
        }),
        next_id: AtomicU64::new(1),
        orphan_policy: OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        },
        revision,
        hook_diagnostics: HookDiagnosticsTracker::default(),
    };

    // Missed SessionStart: PreToolUse with matching cwd binds and moves to Working.
    let mut pre = hook_event(HookEventKind::PreToolUse);
    pre.cwd = Some(session_cwd.clone());
    pre.tool_name = Some("exec".to_owned());
    assert!(
        host.apply_hook_event(WorkerKind::Codex, TEST_PROVIDER_SESSION_ID, pre)
            .unwrap()
    );

    // Subsequent Stop transitions Codex to Idle/Done cleanly.
    let mut stop = hook_event(HookEventKind::Stop);
    stop.cwd = Some(session_cwd.clone());
    assert!(
        host.apply_hook_event(WorkerKind::Codex, TEST_PROVIDER_SESSION_ID, stop)
            .unwrap()
    );
    let state = session.state().unwrap();
    assert_eq!(state.phase, SessionPhase::Idle);
    assert_eq!(state.activity, HookActivity::Done);
    assert_eq!(state.hook_event, Some(HookEventKind::Stop));

    // A second unrelated Codex provider_session_id without an unbound session is unmatched
    // and must NOT steal the already-bound session.
    let mut stray = hook_event(HookEventKind::Stop);
    stray.cwd = Some(session_cwd);
    assert!(
        !host
            .apply_hook_event(
                WorkerKind::Codex,
                "99999999-9999-4999-8999-999999999999",
                stray,
            )
            .unwrap()
    );
    let diag = host.hook_diagnostics();
    assert_eq!(diag.accepted_count, 2);
    assert_eq!(diag.unmatched_count, 1);
    assert_eq!(diag.last_unmatched_worker, Some(WorkerKind::Codex));
    assert_eq!(diag.last_unmatched_event, Some(HookEventKind::Stop));
}

#[test]
fn raw_navigation_does_not_mark_a_turn_running() {
    assert!(!raw_input_starts_turn(b"hello"));
    assert!(!raw_input_starts_turn(b"\x1b[A"));
    assert!(raw_input_starts_turn(b"hello\r"));
    assert!(!raw_input_starts_turn(&[0x03]));
}

#[test]
fn host_revision_bumps_on_touch_client_and_waiters_observe_it() {
    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 120_000,
        grace_ms: 600_000,
    });
    let seen = host.revision();
    host.touch_client("codex-thread-42").unwrap();
    assert_ne!(host.revision(), seen);
    let advanced = host.wait_revision(seen, Duration::from_millis(50));
    assert_ne!(advanced, seen);
}

#[test]
fn host_revision_bumps_when_session_output_arrives() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let before = host.revision();
    let session = host.get("gbt-test").unwrap();
    session.append_output(b"hello".to_vec());
    assert_ne!(host.revision(), before);
}

fn apply_frame_commits(cursors: &mut HashMap<String, u64>, frames: &[WebEventsFramePlan]) {
    for frame in frames {
        for (session, cursor) in &frame.cursor_commits {
            cursors.insert(session.clone(), *cursor);
        }
        for session in &frame.cursor_drops {
            cursors.remove(session);
        }
    }
}

#[test]
fn web_events_initial_reset_uses_ansi_snapshot_and_last_cursor() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    session.append_output(b"abc".to_vec());
    let full_ansi = session.state().unwrap().screen_ansi_base64;
    let cursors = HashMap::new();
    let frames = host.plan_web_events(&cursors, true, 1024 * 1024).unwrap();
    assert_eq!(frames.len(), 1);
    let message = &frames[0].message;
    assert_eq!(message.message_type, "sessions");
    assert_eq!(message.sessions.len(), 1);
    assert!(message.sessions[0].screen.is_none());
    assert!(message.sessions[0].screen_ansi_base64.is_empty());
    assert_eq!(message.terminals.len(), 1);
    let entry = &message.terminals[0];
    assert!(entry.reset);
    assert_eq!(entry.cursor, 0);
    assert_eq!(entry.next_cursor, 3);
    assert_eq!(entry.data_base64, full_ansi);
    assert_eq!(frames[0].cursor_commits.get("gbt-test").copied(), Some(3));
}

#[test]
fn web_events_drains_past_64kib_across_bounded_frames() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    let payload = vec![b'x'; MAX_READ_BYTES + 1_024];
    session.append_output(payload.clone());

    let cursors = HashMap::from([("gbt-test".to_owned(), 0_u64)]);
    // Force multi-frame packing; every produced frame must stay in bound.
    let max_frame = 50_000;
    let frames = host.plan_web_events(&cursors, false, max_frame).unwrap();
    assert!(
        frames.len() >= 2,
        "expected multi-frame drain, got {}",
        frames.len()
    );
    for frame in &frames {
        let encoded = serde_json::to_vec(&frame.message).unwrap();
        assert!(
            encoded.len() <= max_frame,
            "frame len {} exceeds bound {}",
            encoded.len(),
            max_frame
        );
    }

    let mut decoded = Vec::new();
    for frame in &frames {
        for entry in &frame.message.terminals {
            assert!(!entry.reset);
            decoded.extend(
                BASE64
                    .decode(&entry.data_base64)
                    .expect("terminal delta must be valid base64"),
            );
        }
    }
    assert_eq!(decoded, payload);

    let mut committed = cursors.clone();
    assert_eq!(committed.get("gbt-test").copied(), Some(0));
    apply_frame_commits(&mut committed, &frames);
    assert_eq!(
        committed.get("gbt-test").copied(),
        Some(payload.len() as u64)
    );
}

#[test]
fn web_events_freeze_end_stops_live_cursor_chase() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    session.append_output(vec![b'a'; 4_096]);
    let cursors = HashMap::from([("gbt-test".to_owned(), 0_u64)]);

    let producer = Arc::clone(&session);
    let running = Arc::new(AtomicBool::new(true));
    let running_flag = Arc::clone(&running);
    let hammer = thread::spawn(move || {
        while running_flag.load(Ordering::Acquire) {
            producer.append_output(vec![b'z'; 8_192]);
            thread::sleep(Duration::from_millis(1));
        }
    });
    // Let the producer run so list/read can observe a moving live cursor.
    thread::sleep(Duration::from_millis(30));
    let frames = host.plan_web_events(&cursors, false, 1024 * 1024).unwrap();
    running.store(false, Ordering::Release);
    hammer.join().unwrap();

    let mut decoded = 0_u64;
    let mut has_reset = false;
    let mut end = 0_u64;
    for frame in &frames {
        for entry in &frame.message.terminals {
            let raw = BASE64.decode(&entry.data_base64).unwrap();
            if entry.reset {
                has_reset = true;
            } else {
                decoded += raw.len() as u64;
            }
            end = end.max(entry.next_cursor);
        }
    }
    // Reset data is a screen snapshot, so its byte length is not a cursor delta.
    assert!(decoded > 0 || has_reset);
    if !has_reset {
        assert_eq!(decoded, end);
    }
    let freeze_end = frames
        .iter()
        .flat_map(|frame| &frame.message.sessions)
        .find(|state| state.session == "gbt-test")
        .unwrap()
        .last_cursor;
    assert_eq!(end, freeze_end);
    let committed = frames
        .iter()
        .filter_map(|frame| frame.cursor_commits.get("gbt-test").copied())
        .max()
        .unwrap();
    assert_eq!(committed, end);
    // Live stream may have advanced further after the frozen batch.
    assert!(session.state().unwrap().last_cursor >= end);
}

#[test]
fn web_events_splits_large_reset_snapshot_with_final_commit_only() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    // Large screen content so a reset ANSI snapshot exceeds a small bound.
    session.append_output(vec![b'R'; 12_000]);
    let full = session.state().unwrap();
    let full_ansi = BASE64
        .decode(&full.screen_ansi_base64)
        .expect("screen ansi");
    assert!(full_ansi.len() > 1_000);

    let cursors = HashMap::new();
    let max_frame = 2_500;
    let frames = host.plan_web_events(&cursors, true, max_frame).unwrap();
    assert!(
        frames.len() >= 2,
        "expected split reset, got {}",
        frames.len()
    );

    let mut reconstructed = Vec::new();
    let mut saw_reset = false;
    let mut commit_frames = 0_usize;
    for (index, frame) in frames.iter().enumerate() {
        let encoded = serde_json::to_vec(&frame.message).unwrap();
        assert!(
            encoded.len() <= max_frame,
            "frame {index} len {} exceeds bound {max_frame}",
            encoded.len()
        );
        if !frame.cursor_commits.is_empty() {
            commit_frames += 1;
        }
        for entry in &frame.message.terminals {
            if entry.reset {
                assert!(!saw_reset, "reset must appear only on the first chunk");
                assert_eq!(index, 0);
                saw_reset = true;
            } else {
                assert!(saw_reset, "continuation before reset");
            }
            reconstructed.extend(BASE64.decode(&entry.data_base64).unwrap());
        }
    }
    assert!(saw_reset);
    assert_eq!(reconstructed, full_ansi);
    assert_eq!(
        commit_frames, 1,
        "PTY cursor commits only on the final chunk"
    );
    assert_eq!(
        frames
            .last()
            .and_then(|frame| frame.cursor_commits.get("gbt-test").copied()),
        Some(full.last_cursor)
    );
    // Mid-send failure simulation: durable map stays uncommitted until apply.
    assert!(cursors.is_empty());
    let mut durable = cursors.clone();
    apply_frame_commits(&mut durable, &frames[..frames.len() - 1]);
    assert!(
        !durable.contains_key("gbt-test"),
        "partial send must not commit the PTY cursor"
    );
    apply_frame_commits(&mut durable, &frames[frames.len() - 1..]);
    assert_eq!(durable.get("gbt-test").copied(), Some(full.last_cursor));
}

#[test]
fn web_events_sessions_only_oversize_is_a_planning_error() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let cursors = HashMap::new();
    // Bound smaller than any sessions metadata JSON.
    let err = host
        .plan_web_events(&cursors, false, 8)
        .expect_err("sessions-only oversize must fail planning");
    assert!(
        err.to_string().contains("sessions metadata exceeds"),
        "{err:#}"
    );
}

#[test]
fn web_events_resets_when_client_cursor_is_truncated() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let session = host.get("gbt-test").unwrap();
    // Exceed the bounded transcript so cursor 0 becomes truncated.
    let big = vec![b'y'; MAX_TRANSCRIPT_BYTES + 4_096];
    session.append_output(big);
    let last_cursor = session.state().unwrap().last_cursor;

    let cursors = HashMap::from([("gbt-test".to_owned(), 0_u64)]);
    let frames = host.plan_web_events(&cursors, false, 1024 * 1024).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].message.terminals.len(), 1);
    assert!(frames[0].message.terminals[0].reset);
    assert_eq!(frames[0].message.terminals[0].next_cursor, last_cursor);
    assert_eq!(
        frames[0].cursor_commits.get("gbt-test").copied(),
        Some(last_cursor)
    );
    assert_eq!(cursors.get("gbt-test").copied(), Some(0));
}

#[test]
fn web_events_resets_for_new_sessions_and_drops_closed_cursors() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Running);
    let cursors = HashMap::from([("stale-session".to_owned(), 9_u64)]);
    let frames = host.plan_web_events(&cursors, false, 1024 * 1024).unwrap();
    assert_eq!(frames[0].message.terminals.len(), 1);
    assert!(frames[0].message.terminals[0].reset);
    assert!(
        frames[0]
            .cursor_drops
            .iter()
            .any(|session| session == "stale-session")
    );
    assert!(frames[0].cursor_commits.contains_key("gbt-test"));
    assert_eq!(cursors.get("stale-session").copied(), Some(9));
}

#[test]
fn lease_deadline_and_client_state_transition_at_expiry() {
    let host = test_host(TEST_PROVIDER_SESSION_ID, SessionPhase::Idle);
    let lease = Arc::new(AtomicU64::new(1_000));
    {
        let session = host.get("gbt-test").unwrap();
        let mut inner = session.inner.lock().unwrap();
        inner.client_session_id = Some("codex-thread".to_owned());
        inner.client_lease = Some(Arc::clone(&lease));
        inner.orphan_policy = OrphanPolicy {
            lease_ms: 100,
            grace_ms: 200,
        };
        inner.phase_changed_at_ms = 900;
    }
    // last_seen=1000, lease_ms=100 => lease_expires_at=1100.
    // Connected while now < 1100; at 1100 state is already Orphaned (idle).
    // Never call next_lifecycle_deadline_ms while holding inner (non-reentrant Mutex).
    let session = host.get("gbt-test").unwrap();

    let connected_state = session.inner.lock().unwrap().to_state(1_050, false);
    assert_eq!(connected_state.client_lease_ms, Some(100));
    assert_eq!(connected_state.orphan_grace_ms, Some(200));
    assert_eq!(connected_state.client_state, ClientLeaseState::Connected);

    assert_eq!(
        session
            .inner
            .lock()
            .unwrap()
            .client_lifecycle(1_099, false)
            .0,
        ClientLeaseState::Connected
    );
    assert_eq!(
        session.next_lifecycle_deadline_ms(1_099).unwrap(),
        Some(1_100)
    );

    assert_eq!(
        session
            .inner
            .lock()
            .unwrap()
            .client_lifecycle(1_100, false)
            .0,
        ClientLeaseState::Orphaned
    );
    assert_eq!(session.next_lifecycle_deadline_ms(1_100).unwrap(), None);

    assert_eq!(
        session
            .inner
            .lock()
            .unwrap()
            .client_lifecycle(1_101, false)
            .0,
        ClientLeaseState::Orphaned
    );
    assert_eq!(session.next_lifecycle_deadline_ms(1_101).unwrap(), None);

    // Exactly one due wake at expiry observes the changed state: deadline was
    // scheduled while Connected, and the first moment now >= deadline lists
    // Orphaned with no further pure-time deadline.
    let scheduled = session.next_lifecycle_deadline_ms(1_050).unwrap();
    assert_eq!(scheduled, Some(1_100));
    let due_now = scheduled.unwrap();
    let due_state = session
        .inner
        .lock()
        .unwrap()
        .client_lifecycle(due_now, false)
        .0;
    assert_eq!(due_state, ClientLeaseState::Orphaned);
    assert_eq!(session.next_lifecycle_deadline_ms(due_now).unwrap(), None);
}

#[cfg(windows)]
#[test]
fn windows_process_tree_helper() {
    let Some(role) = env::var_os(WINDOWS_PROCESS_TREE_ROLE_ENV) else {
        return;
    };
    let directory = PathBuf::from(
        env::var_os(WINDOWS_PROCESS_TREE_DIR_ENV)
            .expect("process-tree helper directory must be provided"),
    );
    let role = role.to_string_lossy();
    match role.as_ref() {
        "wrapper" => {
            let gate_name = env::var_os(WINDOWS_PROCESS_TREE_GATE_ENV)
                .expect("wrapper launch gate must be provided");
            wait_for_windows_launch_gate(&gate_name, WINDOWS_LAUNCH_HANDSHAKE_TIMEOUT_MS)
                .expect("wrapper launch gate must open");
            let mut child = spawn_windows_process_tree_helper(
                "short-child",
                &directory,
                std::process::id(),
                None,
            );
            child.wait().expect("short-lived child must exit");
            fs::write(directory.join("wrapper-exited"), b"ready")
                .expect("wrapper exit marker must be written");
        }
        "short-child" => {
            let root_pid = env::var(WINDOWS_PROCESS_TREE_ROOT_ENV)
                .expect("root pid must be provided")
                .parse::<u32>()
                .expect("root pid must be numeric");
            let grandchild =
                spawn_windows_process_tree_helper("grandchild", &directory, root_pid, None);
            fs::write(directory.join("child.pid"), std::process::id().to_string())
                .expect("child pid marker must be written");
            fs::write(
                directory.join("grandchild.pid"),
                grandchild.id().to_string(),
            )
            .expect("grandchild pid marker must be written");
            fs::write(
                directory.join("ready"),
                format!("{root_pid} {} {}\n", std::process::id(), grandchild.id()),
            )
            .expect("ready marker must be written");
            drop(grandchild);
        }
        "grandchild" => thread::sleep(Duration::from_secs(25)),
        unexpected => panic!("unexpected Windows process-tree helper role: {unexpected}"),
    }
}

#[cfg(windows)]
#[test]
fn windows_session_close_contains_descendant_after_intermediates_exit() {
    use std::os::windows::io::AsRawHandle;

    let directory = temporary_test_directory("windows-job-tree");
    fs::create_dir_all(&directory).unwrap();
    let gate = WindowsLaunchGate::create().unwrap();
    let job = WindowsJob::create().unwrap();
    let root = spawn_windows_process_tree_helper(
        "wrapper",
        &directory,
        0,
        Some(gate.event_name.as_os_str()),
    );
    let root_pid = root.id();
    job.assign_process(root.as_raw_handle()).unwrap();
    gate.signal().unwrap();
    let mut cleanup = WindowsProcessTreeCleanup::new(root, directory.clone());

    let pids = wait_for_windows_process_tree_ready(&directory.join("ready"));
    cleanup.remember(&pids);
    assert_eq!(pids[0], root_pid, "ready marker reported another root");
    assert_ne!(pids[0], pids[1]);
    assert_ne!(pids[1], pids[2]);
    wait_for_windows_marker(&directory.join("wrapper-exited"));
    // The marker only proves the wrapper wrote it; the wrapper and
    // short-lived child processes may still be tearing down. Wait for
    // both to actually exit before asserting Gone, while the grandchild
    // and the job must remain alive.
    wait_for_windows_intermediates_exit([pids[0], pids[1]]);
    assert_eq!(process_alive(pids[2]), ScopeAlive::Alive);
    assert_eq!(job.process_scope_alive(), ScopeAlive::Alive);

    let host = windows_process_host(root_pid, job);
    assert!(host.close("gbt-test").unwrap());
    wait_for_windows_process_tree_exit(pids);
    assert!(host.show("gbt-test").is_err());
    assert!(
        host.close("gbt-test").unwrap(),
        "tombstone re-close must succeed"
    );
    cleanup.disarm();
}

/// Spawn the wrapper/short-child/grandchild process tree, assign the
/// wrapper to a fresh Job Object, and wait until the wrapper and its
/// short-lived child have exited so only the long-lived grandchild remains
/// in the job. Returns the job, the three pids, and the armed cleanup
/// guard.
#[cfg(windows)]
fn spawn_windows_tree_in_job(label: &str) -> (WindowsJob, [u32; 3], WindowsProcessTreeCleanup) {
    use std::os::windows::io::AsRawHandle;

    let directory = temporary_test_directory(label);
    fs::create_dir_all(&directory).unwrap();
    let gate = WindowsLaunchGate::create().unwrap();
    let job = WindowsJob::create().unwrap();
    let root = spawn_windows_process_tree_helper(
        "wrapper",
        &directory,
        0,
        Some(gate.event_name.as_os_str()),
    );
    job.assign_process(root.as_raw_handle()).unwrap();
    gate.signal().unwrap();
    let mut cleanup = WindowsProcessTreeCleanup::new(root, directory.clone());
    let pids = wait_for_windows_process_tree_ready(&directory.join("ready"));
    cleanup.remember(&pids);
    wait_for_windows_marker(&directory.join("wrapper-exited"));
    wait_for_windows_intermediates_exit([pids[0], pids[1]]);
    (job, pids, cleanup)
}

/// The Windows escalation ladder must match its documented semantics: Hup
/// and Term are wait-only phases that never TerminateJobObject, and only
/// Kill force-terminates the whole job scope.
#[cfg(windows)]
#[test]
fn windows_job_terminate_waits_on_hup_and_term_until_kill() {
    let (job, pids, mut cleanup) = spawn_windows_tree_in_job("windows-job-phases");
    assert_eq!(job.process_scope_alive(), ScopeAlive::Alive);

    job.terminate(TerminationLevel::Hup).unwrap();
    assert_eq!(
        job.process_scope_alive(),
        ScopeAlive::Alive,
        "Hup is a wait-only phase and must not terminate the job"
    );
    job.terminate(TerminationLevel::Term).unwrap();
    assert_eq!(
        job.process_scope_alive(),
        ScopeAlive::Alive,
        "Term is a wait-only phase and must not terminate the job"
    );

    job.terminate(TerminationLevel::Kill).unwrap();
    wait_for_windows_process_tree_exit(pids);
    assert_eq!(job.process_scope_alive(), ScopeAlive::Gone);
    cleanup.disarm();
}

/// The bare-process fallback (`terminate_windows_process`) must mirror the
/// job semantics: Hup and Term wait, Kill force-terminates.
#[cfg(windows)]
#[test]
fn windows_bare_process_terminate_waits_on_hup_and_term_until_kill() {
    let directory = temporary_test_directory("windows-bare-process-phases");
    fs::create_dir_all(&directory).unwrap();
    let child = spawn_windows_process_tree_helper("grandchild", &directory, 0, None);
    let pid = child.id();
    let mut cleanup = WindowsProcessTreeCleanup::new(child, directory.clone());

    terminate_windows_process(pid, TerminationLevel::Hup).unwrap();
    assert_eq!(
        process_alive(pid),
        ScopeAlive::Alive,
        "Hup is a wait-only phase and must not terminate the process"
    );
    terminate_windows_process(pid, TerminationLevel::Term).unwrap();
    assert_eq!(
        process_alive(pid),
        ScopeAlive::Alive,
        "Term is a wait-only phase and must not terminate the process"
    );

    terminate_windows_process(pid, TerminationLevel::Kill).unwrap();
    let deadline = Instant::now() + WINDOWS_PROCESS_TREE_TIMEOUT;
    while process_alive(pid) != ScopeAlive::Gone {
        assert!(
            Instant::now() < deadline,
            "bare Windows process {pid} survived TerminateProcess"
        );
        thread::sleep(Duration::from_millis(10));
    }
    cleanup.disarm();
}

/// `shutdown_all` must force-terminate the Job Object scope even when the
/// bounded group close times out waiting for the close lock, so server
/// exit never leaves a Windows Grok descendant behind.
#[cfg(windows)]
#[test]
fn windows_shutdown_all_force_kills_the_job_scope_after_a_close_timeout() {
    let (job, pids, mut cleanup) = spawn_windows_tree_in_job("windows-shutdown-all-timeout");
    let host = windows_process_host(pids[0], job);
    let session = host.get("gbt-test").unwrap();
    // Hold the close lock so close_attempt spins until the shared deadline
    // and reports Timeout; shutdown_all's final forced pass must still
    // terminate the job scope.
    let _held = session.test_hold_close_lock();

    assert_eq!(process_alive(pids[2]), ScopeAlive::Alive);
    host.shutdown_all().unwrap();
    wait_for_windows_process_tree_exit(pids);
    cleanup.disarm();
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn all_zombie_scope_requires_stable_membership() {
    assert_eq!(
        stable_process_group_scan_result(
            (ScopeAlive::Gone, vec![41]),
            (ScopeAlive::Gone, vec![41]),
        ),
        ScopeAlive::Gone
    );
    assert_eq!(
        stable_process_group_scan_result(
            (ScopeAlive::Gone, vec![41]),
            (ScopeAlive::Gone, vec![41, 42]),
        ),
        ScopeAlive::Unknown
    );
    assert_eq!(
        stable_process_group_scan_result(
            (ScopeAlive::Gone, vec![41]),
            (ScopeAlive::Alive, vec![41, 42]),
        ),
        ScopeAlive::Alive
    );
}

#[test]
#[cfg(unix)]
fn unix_scope_probe_tracks_descendant_until_group_kill() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let directory = temporary_test_directory("unix-process-group");
    fs::create_dir_all(&directory).unwrap();
    let descendant_pid_path = directory.join("descendant-pid");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "(trap '' HUP TERM; sleep 30) & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
            "grok-bridge-unix-process-group-test",
        ])
        .arg(&descendant_pid_path);
    // portable-pty uses setsid for the child. Mirror that ownership model
    // so the root pid is also the process-group id used by the probe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut root = command.spawn().unwrap();
    let pgid = root.id();
    root.wait().unwrap();
    let descendant_pid = fs::read_to_string(&descendant_pid_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();

    assert_eq!(process_scope_alive(pgid, true), ScopeAlive::Alive);
    let deadline = default_close_deadline();
    let mut scope_gone = false;
    while Instant::now() < deadline {
        if process_scope_alive(pgid, true) == ScopeAlive::Gone {
            scope_gone = true;
            break;
        }
        send_termination_signal(pgid, TerminationLevel::Kill).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        scope_gone,
        "process group {pgid} remained live after repeated SIGKILL"
    );
    assert_eq!(
        unix_test_process_alive(descendant_pid),
        ScopeAlive::Gone,
        "tracked descendant {descendant_pid} remained executable"
    );
    fs::remove_dir_all(directory).unwrap();
}

/// Spawn a setsid'd shell that exits immediately while a descendant keeps
/// running in the same process group, ignoring HUP and TERM. Returns the
/// group id (the leader pid) and the descendant pid.
#[cfg(unix)]
fn spawn_unix_process_group_with_descendant(directory: &Path) -> (std::process::Child, u32, u32) {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let descendant_pid_path = directory.join("descendant-pid");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "(trap '' HUP TERM; sleep 30) & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
            "grok-bridge-shutdown-all-test",
        ])
        .arg(&descendant_pid_path);
    // portable-pty uses setsid for the child. Mirror that ownership model
    // so the root pid is also the process-group id used by the probe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut root = command.spawn().unwrap();
    let pgid = root.id();
    root.wait().unwrap();
    let descendant_pid = fs::read_to_string(&descendant_pid_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    (root, pgid, descendant_pid)
}

/// `shutdown_all` must force-kill and reap a surviving process group even
/// when the bounded group close fails (poisoned close lock): server exit
/// must never orphan a Unix Grok process or its descendants.
#[test]
#[cfg(unix)]
fn shutdown_all_force_kills_the_process_group_after_a_close_failure() {
    let directory = temporary_test_directory("shutdown-all-failed");
    fs::create_dir_all(&directory).unwrap();
    let (mut root, pgid, descendant_pid) = spawn_unix_process_group_with_descendant(&directory);

    let host = unix_process_host(pgid);
    let session = host.get("gbt-test").unwrap();
    // Poison the close lock so close_attempt fails fast and deterministically;
    // shutdown_all must still terminate and reap the whole owned scope.
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.test_poison_close_lock();
    }));
    assert!(poisoned.is_err());

    host.shutdown_all().unwrap();
    assert_eq!(
        process_scope_alive(pgid, true),
        ScopeAlive::Gone,
        "shutdown_all must reap the whole process group after a failed close"
    );
    assert_eq!(
        unix_test_process_alive(descendant_pid),
        ScopeAlive::Gone,
        "shutdown_all must reap the descendant after a failed close"
    );
    let _ = root.kill();
    let _ = root.wait();
    fs::remove_dir_all(directory).unwrap();
}

/// `shutdown_all` must force-kill and reap a surviving process group even
/// when the bounded group close times out waiting for the close lock.
#[test]
#[cfg(unix)]
fn shutdown_all_force_kills_the_process_group_after_a_close_timeout() {
    let directory = temporary_test_directory("shutdown-all-timeout");
    fs::create_dir_all(&directory).unwrap();
    let (mut root, pgid, descendant_pid) = spawn_unix_process_group_with_descendant(&directory);

    let host = unix_process_host(pgid);
    let session = host.get("gbt-test").unwrap();
    // Hold the close lock so close_attempt spins until the shared deadline
    // and reports Timeout; the session keeps ownership and shutdown_all's
    // final forced pass must still terminate and reap the whole group.
    let _held = session.test_hold_close_lock();

    host.shutdown_all().unwrap();
    assert_eq!(
        process_scope_alive(pgid, true),
        ScopeAlive::Gone,
        "shutdown_all must reap the whole process group after a close timeout"
    );
    assert_eq!(
        unix_test_process_alive(descendant_pid),
        ScopeAlive::Gone,
        "shutdown_all must reap the descendant after a close timeout"
    );
    let _ = root.kill();
    let _ = root.wait();
    fs::remove_dir_all(directory).unwrap();
}

/// Every surviving session must get its own forced-termination window:
/// with two independent process groups whose closes all fail, `shutdown_all`
/// must Kill and reap both instead of sharing one deadline that a slow first
/// scope could exhaust.
#[test]
#[cfg(unix)]
fn shutdown_all_force_kills_every_group_when_all_closes_fail() {
    let directory = temporary_test_directory("shutdown-all-multi");
    fs::create_dir_all(&directory).unwrap();
    let first_dir = directory.join("first");
    let second_dir = directory.join("second");
    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();
    let (mut root_a, pgid_a, descendant_a) = spawn_unix_process_group_with_descendant(&first_dir);
    let (mut root_b, pgid_b, descendant_b) = spawn_unix_process_group_with_descendant(&second_dir);
    assert_ne!(pgid_a, pgid_b);
    let host = unix_process_hosts(&[pgid_a, pgid_b]);
    // Fail both close paths fast and deterministically; shutdown_all must
    // still force-kill and reap both owned scopes.
    for handle in ["gbt-1", "gbt-2"] {
        let session = host.get(handle).unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.test_poison_close_lock();
        }));
        assert!(poisoned.is_err(), "{handle} close lock must be poisoned");
    }

    host.shutdown_all().unwrap();
    assert_eq!(process_scope_alive(pgid_a, true), ScopeAlive::Gone);
    assert_eq!(process_scope_alive(pgid_b, true), ScopeAlive::Gone);
    assert_eq!(unix_test_process_alive(descendant_a), ScopeAlive::Gone);
    assert_eq!(unix_test_process_alive(descendant_b), ScopeAlive::Gone);
    let _ = root_a.kill();
    let _ = root_a.wait();
    let _ = root_b.kill();
    let _ = root_b.wait();
    fs::remove_dir_all(directory).unwrap();
}

/// True once `pid` has exited but is still an unreaped zombie held by this
/// test process. Linux reads the state field from /proc; macOS reports no
/// process info for an exited-but-unreaped child (proc_pidinfo returns 0).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn test_unreaped_zombie(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.split_whitespace().next() == Some("Z")),
            Err(_) => false,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let read = unsafe {
            libc::proc_pidinfo(
                pid as libc::pid_t,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of_val(&info) as libc::c_int,
            )
        };
        read == 0
    }
}

/// A full terminal-response queue is transient backpressure: the error is
/// recorded, but the owned process scope must never be escalated.
#[test]
#[cfg(unix)]
fn full_terminal_response_queue_does_not_terminate_the_process_scope() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut root = Command::new("/bin/sh");
    root.args(["-c", "exec sleep 60", "grok-bridge-queue-backpressure-test"]);
    unsafe {
        root.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut root = root.spawn().unwrap();
    let pgid = root.id();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && process_scope_alive(pgid, true) != ScopeAlive::Alive {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(process_scope_alive(pgid, true), ScopeAlive::Alive);

    // A zero-capacity writer queue makes the next enqueue report Full.
    let mut session = test_session(SessionPhase::Running);
    session.scope_id = pgid;
    let (writer_tx, _writer_rx) = sync_channel(0);
    *session.writer_tx.lock().unwrap() = Some(writer_tx);

    session.queue_terminal_response(vec![0x1b; 64]);
    assert!(
        session
            .inner
            .lock()
            .unwrap()
            .error
            .as_deref()
            .is_some_and(|error| error.contains("queue is full")),
        "queue backpressure must be recorded for observers"
    );
    assert_eq!(session.state().unwrap().phase, SessionPhase::Running);
    assert_eq!(
        process_scope_alive(pgid, true),
        ScopeAlive::Alive,
        "queue backpressure must not escalate the owned process scope"
    );

    // Cleanup: the scope is killed only by an explicit termination signal,
    // then reaped so the probe can observe the group empty.
    send_termination_signal(pgid, TerminationLevel::Kill).unwrap();
    let _ = root.wait();
    let deadline = default_close_deadline();
    let mut scope_gone = false;
    while Instant::now() < deadline {
        if process_scope_alive(pgid, true) == ScopeAlive::Gone {
            scope_gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(scope_gone, "process group {pgid} survived cleanup SIGKILL");
}

/// The Linux fast kill(0) probe must answer Alive without the /proc scan,
/// while the stable probe still distinguishes an all-zombie group; macOS
/// keeps its existing always-scan behavior (an unreaped zombie answers
/// kill(0) with EPERM, mapped to Alive as before).
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fast_scope_probe_skips_the_scan_outside_the_stable_confirmation_window() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut child = Command::new("/bin/sh");
    child.args(["-c", "exec sleep 60", "grok-bridge-fast-probe-test"]);
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn().unwrap();
    let pgid = child.id();

    // Live group: both probe modes agree the scope is alive.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && process_scope_alive(pgid, true) != ScopeAlive::Alive {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(process_scope_alive(pgid, true), ScopeAlive::Alive);
    assert_eq!(process_scope_alive(pgid, false), ScopeAlive::Alive);

    // SIGKILL leaves the child as an unreaped zombie held by this test.
    send_termination_signal(pgid, TerminationLevel::Kill).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !test_unreaped_zombie(pgid) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        test_unreaped_zombie(pgid),
        "process {pgid} never became a zombie"
    );
    #[cfg(target_os = "linux")]
    {
        // The fast probe answers from kill(0) alone: Alive when the
        // zombie group still answers the existence probe, Gone when it
        // no longer does — never a /proc walk.
        let fast = process_scope_alive(pgid, false);
        let kill0_succeeds = unsafe { libc::kill(-(pgid as libc::pid_t), 0) } == 0;
        assert_eq!(
            fast == ScopeAlive::Alive,
            kill0_succeeds,
            "fast probe must mirror the kill(0) result"
        );
        // The stable probe certifies the all-zombie group as gone.
        assert_eq!(process_scope_alive(pgid, true), ScopeAlive::Gone);
    }
    #[cfg(target_os = "macos")]
    assert_eq!(
        process_scope_alive(pgid, true),
        ScopeAlive::Alive,
        "macOS keeps mapping an unreaped zombie to Alive"
    );

    // Reaping the zombie empties the group: both probes now report Gone.
    child.wait().unwrap();
    assert_eq!(process_scope_alive(pgid, true), ScopeAlive::Gone);
    assert_eq!(process_scope_alive(pgid, false), ScopeAlive::Gone);
}

#[cfg(target_os = "macos")]
fn unix_test_process_alive(pid: u32) -> ScopeAlive {
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of_val(&info) as libc::c_int,
        )
    };
    if read == std::mem::size_of_val(&info) as libc::c_int {
        if info.pbi_status == libc::SZOMB {
            ScopeAlive::Gone
        } else {
            ScopeAlive::Alive
        }
    } else if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        ScopeAlive::Gone
    } else {
        ScopeAlive::Unknown
    }
}

#[cfg(target_os = "linux")]
fn unix_test_process_alive(pid: u32) -> ScopeAlive {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().next())
        {
            Some("Z" | "X") => ScopeAlive::Gone,
            Some(_) => ScopeAlive::Alive,
            None => ScopeAlive::Unknown,
        },
        Err(error) if error.kind() == ErrorKind::NotFound => ScopeAlive::Gone,
        Err(_) => ScopeAlive::Unknown,
    }
}

#[test]
fn tombstone_eviction_keeps_recent_handles() {
    let mut registry = SessionRegistry {
        accepting: true,
        sessions: HashMap::new(),
        provider_sessions: HashMap::new(),
        external_bound_handles: HashSet::new(),
        clients: HashMap::new(),
        closed: HashMap::new(),
    };
    let now = now_millis();
    for index in 0..CLOSED_TOMBSTONE_CAP {
        registry.closed.insert(
            format!("old-{index}"),
            now.saturating_sub(CLOSED_TOMBSTONE_TTL_MS + 1),
        );
    }
    registry.closed.insert("recent".to_owned(), now);
    registry.remember_closed("newest");
    assert!(registry.was_closed("recent"));
    assert!(registry.was_closed("newest"));
    assert!(!registry.was_closed("old-0"));
}
