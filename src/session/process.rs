use super::*;

/// Highest Unix process-group signal already sent for this session.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum TerminationLevel {
    #[default]
    None,
    Hup,
    Term,
    Kill,
}

#[cfg(windows)]
#[derive(Clone)]
pub(super) struct WindowsJob {
    handle: Arc<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
pub(super) struct WindowsLaunchGate {
    pub(super) event_name: OsString,
    event: std::os::windows::io::OwnedHandle,
    pub(super) pid_report_name: OsString,
    pid_report: std::os::windows::io::OwnedHandle,
}

/// Result of a liveness probe of the owned process scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScopeAlive {
    /// The whole owned process scope is verifiably gone.
    Gone,
    /// The owned process scope is still alive.
    Alive,
    /// Liveness could not be verified (lock poisoned, or Windows could not
    /// open the process). Never treated as `Gone`.
    Unknown,
}

/// Session-level classification of the owned process scope for escalation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScopeState {
    /// Session terminal or the whole owned scope verifiably gone.
    Gone,
    /// The owned scope is still alive.
    Alive,
    /// Cannot verify (probe inconclusive or no retained pid).
    Unknown,
}

pub(super) enum EscalationResult {
    /// The process is verified terminated (waiter observed its exit).
    Done,
    /// The absolute deadline passed with the process still live.
    Timeout,
    /// Sending the termination signal failed.
    Failed(String),
}

#[cfg(windows)]
pub(super) fn build_windows_launcher_command(
    config: &LaunchConfig,
    provider_session_id: &str,
    state_dir: Option<&Path>,
) -> Result<(CommandBuilder, WindowsLaunchGate, WindowsJob)> {
    let worker_cmd = worker::build_worker_command(
        config.worker,
        &config.bin,
        &config.cwd,
        config.prompt.as_deref(),
        config.model.as_deref(),
        config.always_approve,
        config.sandbox,
        config.dangerously_bypass,
        provider_session_id,
        state_dir,
    );
    let gate = WindowsLaunchGate::create()?;
    let job = WindowsJob::create().context("failed to create the Windows worker Job Object")?;
    let executable = env::current_exe().context("failed to locate the agent-bridge executable")?;
    let mut launcher = CommandBuilder::new(executable);
    launcher.cwd(config.cwd.as_os_str());
    launcher.env("TERM", "xterm-256color");
    launcher.env("COLORTERM", "truecolor");
    worker::strip_parent_host_env(&mut launcher);
    match config.worker {
        WorkerKind::Grok => {
            if let Some(state_dir) = state_dir {
                launcher.env("GROK_HOME", state_dir.as_os_str());
            }
        }
        WorkerKind::Codex => {
            launcher.env("AGENT_BRIDGE_SESSION_ID", provider_session_id);
            launcher.env("AGENT_BRIDGE_WORKER", WorkerKind::Codex.as_str());
        }
    }
    launcher.arg("__windows-job-child");
    launcher.arg(&gate.event_name);
    launcher.arg(&gate.pid_report_name);
    launcher.args(worker_cmd.get_argv());
    Ok((launcher, gate, job))
}

#[cfg(windows)]
pub(crate) fn run_windows_job_child(mut arguments: Vec<OsString>) -> Result<i32> {
    if arguments.len() < 3 {
        bail!("internal Windows job child requires event, PID report, and program names");
    }
    let gate_name = arguments.remove(0);
    let pid_report_name = arguments.remove(0);
    let program = arguments.remove(0);
    wait_for_windows_launch_gate(&gate_name, WINDOWS_LAUNCH_HANDSHAKE_TIMEOUT_MS)?;
    let mut command = std::process::Command::new(&program);
    command.args(arguments);
    worker::strip_parent_host_env_std(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start gated worker process {program:?}"))?;
    report_windows_worker_pid(&pid_report_name, child.id())?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for gated worker process {program:?}"))?;
    Ok(status.code().unwrap_or(1))
}

/// Escalation milestone inside a close budget: `fraction` of the budget, but
/// never sooner than the minimum grace so a graceful child is not
/// force-killed prematurely.
pub(super) fn escalation_milestone(
    budget: Duration,
    fraction_numerator: u64,
    fraction_denominator: u64,
) -> Duration {
    budget
        .mul_f64(fraction_numerator as f64 / fraction_denominator as f64)
        .max(Duration::from_millis(TERMINATION_GRACE_MIN_MS))
}

/// Send the requested termination to the process scope owned by the Session.
///
/// Unix: portable-pty spawns the PTY child as a session leader (`setsid`), so
/// the process group id equals the child's pid; signal the whole group so every
/// current member — including descendants that share the PTY — receives the
/// escalation. `ESRCH` means the group is already gone and is treated as
/// success; the liveness probe (see `process_scope_alive`) verifies the scope.
///
/// Windows: a gated launcher is assigned to a Job Object before Grok starts.
/// Every descendant therefore enters the same kernel-owned scope, even when an
/// intermediate process exits before close begins.
#[cfg(unix)]
pub(super) fn send_termination_signal(pid: u32, level: TerminationLevel) -> std::io::Result<()> {
    let signal = match level {
        TerminationLevel::Hup => libc::SIGHUP,
        TerminationLevel::Term => libc::SIGTERM,
        TerminationLevel::Kill => libc::SIGKILL,
        TerminationLevel::None => return Ok(()),
    };
    // SAFETY: `kill` with a process group id derived from the session leader's
    // pid touches only that group; the child owns it because it called setsid.
    let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // The process group is already gone; the liveness probe observes it.
        Ok(())
    } else {
        Err(error)
    }
}

/// Probe whether the Unix process group owned by the Session is still alive.
/// The group id equals the session leader pid (`setsid` in portable-pty), and
/// the group persists as long as any member — including a descendant that
/// retained the PTY after the root exited — remains. Only `ESRCH` proves the
/// whole scope is gone; `EPERM` means it exists but is not signalable (still
/// alive), and any other error leaves the answer unknown.
///
/// `stable_verification` enables the all-zombie distinction on Linux: an
/// all-zombie group can still answer kill(-pgid, 0) with success (EPERM on
/// macOS), so a successful existence probe alone cannot certify `Gone`; the
/// stable two-scan view is required. Escalation asks for that only in its
/// final confirmation window (`STABLE_SCOPE_SCAN_LEAD_MS`); outside it a
/// successful existence probe answers `Alive` without walking /proc.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub(super) fn process_scope_alive(pid: u32, stable_verification: bool) -> ScopeAlive {
    // SAFETY: signal 0 only probes existence; `-pid` addresses the group owned
    // by the session leader that called setsid, so no unrelated group is hit.
    let result = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    if result == 0 {
        #[cfg(target_os = "macos")]
        return macos_process_group_alive(pid);
        #[cfg(target_os = "linux")]
        return linux_process_group_alive(pid, stable_verification);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return ScopeAlive::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ScopeAlive::Gone,
        Some(libc::EPERM) => ScopeAlive::Alive,
        _ => ScopeAlive::Unknown,
    }
}

/// `kill(-pgid, 0)` also succeeds when a group contains only zombies. Require
/// two all-zombie views with identical membership before treating the group as
/// terminated: enumeration and per-PID inspection are not one atomic snapshot.
#[cfg(target_os = "macos")]
fn macos_process_group_alive(pgid: u32) -> ScopeAlive {
    stable_process_group_scan_result(
        macos_process_group_scan(pgid),
        macos_process_group_scan(pgid),
    )
}

#[cfg(target_os = "macos")]
fn macos_process_group_scan(pgid: u32) -> (ScopeAlive, Vec<u32>) {
    const MAX_GROUP_MEMBERS: usize = 4096;
    let mut pids = vec![0_i32; MAX_GROUP_MEMBERS];
    let buffer_bytes = pids.len() * std::mem::size_of::<libc::pid_t>();
    let returned = unsafe {
        libc::proc_listpgrppids(
            pgid as libc::pid_t,
            pids.as_mut_ptr().cast(),
            buffer_bytes as libc::c_int,
        )
    };
    if returned < 0 || returned as usize >= pids.len() {
        return (ScopeAlive::Unknown, Vec::new());
    }
    let count = returned as usize;
    let mut members = pids
        .into_iter()
        .take(count)
        .filter(|pid| *pid > 0)
        .map(|pid| pid as u32)
        .collect::<Vec<_>>();
    members.sort_unstable();
    members.dedup();
    let mut verdict = ScopeAlive::Gone;
    for &process_id in &members {
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let read = unsafe {
            libc::proc_pidinfo(
                process_id as libc::pid_t,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of_val(&info) as libc::c_int,
            )
        };
        if read != std::mem::size_of_val(&info) as libc::c_int {
            verdict = ScopeAlive::Unknown;
            continue;
        }
        if info.pbi_pgid != pgid {
            verdict = ScopeAlive::Unknown;
            continue;
        }
        if info.pbi_status != libc::SZOMB {
            return (ScopeAlive::Alive, members);
        }
    }
    (verdict, members)
}

/// Linux exposes process-group and state fields in `/proc/<pid>/stat`. When
/// `stable_verification` is set, scan twice and require stable membership
/// before certifying an all-zombie group; otherwise a successful kill(0) probe
/// already proved the group exists and a living member answers `Alive`
/// immediately, skipping the /proc traversal.
#[cfg(target_os = "linux")]
fn linux_process_group_alive(pgid: u32, stable_verification: bool) -> ScopeAlive {
    if !stable_verification {
        return ScopeAlive::Alive;
    }
    stable_process_group_scan_result(
        linux_process_group_scan(pgid),
        linux_process_group_scan(pgid),
    )
}

#[cfg(target_os = "linux")]
fn linux_process_group_scan(pgid: u32) -> (ScopeAlive, Vec<u32>) {
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return (ScopeAlive::Unknown, Vec::new()),
    };
    let mut members = Vec::new();
    let mut verdict = ScopeAlive::Gone;
    for entry in entries {
        let Ok(entry) = entry else {
            verdict = ScopeAlive::Unknown;
            continue;
        };
        let Some(process_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(_) => {
                verdict = ScopeAlive::Unknown;
                continue;
            }
        };
        let Some(after_name) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
            verdict = ScopeAlive::Unknown;
            continue;
        };
        let mut fields = after_name.split_whitespace();
        let state = fields.next();
        let _parent_pid = fields.next();
        let process_group = fields.next().and_then(|field| field.parse::<u32>().ok());
        if process_group != Some(pgid) {
            continue;
        }
        members.push(process_id);
        match state {
            Some("Z" | "X") => {}
            Some(_) => {
                members.sort_unstable();
                return (ScopeAlive::Alive, members);
            }
            None => verdict = ScopeAlive::Unknown,
        }
    }
    members.sort_unstable();
    members.dedup();
    (verdict, members)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn stable_process_group_scan_result(
    first: (ScopeAlive, Vec<u32>),
    second: (ScopeAlive, Vec<u32>),
) -> ScopeAlive {
    if first.0 != ScopeAlive::Gone {
        return first.0;
    }
    if second.0 != ScopeAlive::Gone {
        return second.0;
    }
    if first.1 == second.1 {
        ScopeAlive::Gone
    } else {
        ScopeAlive::Unknown
    }
}

#[cfg(windows)]
impl WindowsJob {
    pub(super) fn create() -> std::io::Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            handle: Arc::new(handle),
        })
    }

    pub(super) fn assign_process(
        &self,
        process: std::os::windows::io::RawHandle,
    ) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        if unsafe { AssignProcessToJobObject(self.handle.as_raw_handle() as _, process as _) } == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn process_scope_alive(&self) -> ScopeAlive {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };

        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle() as _,
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            ScopeAlive::Unknown
        } else if info.ActiveProcesses == 0 {
            ScopeAlive::Gone
        } else {
            ScopeAlive::Alive
        }
    }

    /// Apply one termination level to the Job Object scope.
    ///
    /// Windows has no job-scoped graceful signal, so the escalation ladder's
    /// `Hup` and `Term` phases are wait-only: the close loop keeps probing
    /// `ActiveProcesses` and gives members a chance to exit naturally, and no
    /// force-kill is issued. Only `Kill` force-terminates every member with
    /// `TerminateJobObject`. A scope that is already verifiably gone is
    /// success, mirroring the Unix `ESRCH` semantics.
    pub(super) fn terminate(&self, level: TerminationLevel) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        match level {
            TerminationLevel::None => Ok(()),
            TerminationLevel::Hup | TerminationLevel::Term => {
                // Wait-only phase: no TerminateJobObject. The escalation loop
                // owns the bounded wait between milestones; a job whose members
                // already exited is success.
                Ok(())
            }
            TerminationLevel::Kill => {
                if unsafe { TerminateJobObject(self.handle.as_raw_handle() as _, 1) } != 0
                    || self.process_scope_alive() == ScopeAlive::Gone
                {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            }
        }
    }
}

#[cfg(windows)]
impl WindowsLaunchGate {
    pub(super) fn create() -> Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{CreateEventW, CreateSemaphoreW};

        let id = generate_provider_session_id()?;
        let event_name = OsString::from(format!("Local\\agent-bridge-launch-{id}"));
        let pid_report_name = OsString::from(format!("Local\\agent-bridge-pid-{id}"));
        let wide_event_name = windows_wide_null(&event_name);
        // Manual-reset (bManualReset=TRUE) so SetEvent stays signaled if the
        // ConPTY child reaches WaitForSingleObject after the parent released
        // the gate.
        let raw_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_event_name.as_ptr()) };
        if raw_event.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create the Windows launch event");
        }
        let event = unsafe { OwnedHandle::from_raw_handle(raw_event as _) };
        let wide_pid_report_name = windows_wide_null(&pid_report_name);
        let raw_pid_report = unsafe {
            CreateSemaphoreW(std::ptr::null(), 0, i32::MAX, wide_pid_report_name.as_ptr())
        };
        if raw_pid_report.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create the Windows PID report semaphore");
        }
        Ok(Self {
            event_name,
            event,
            pid_report_name,
            pid_report: unsafe { OwnedHandle::from_raw_handle(raw_pid_report as _) },
        })
    }

    pub(super) fn signal(&self) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::SetEvent;

        if unsafe { SetEvent(self.event.as_raw_handle() as _) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn wait_for_worker_pid(
        &self,
        timeout_ms: u32,
        launcher: Option<std::os::windows::io::RawHandle>,
    ) -> Result<u32> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                GetExitCodeProcess, ReleaseSemaphore, WaitForMultipleObjects, WaitForSingleObject,
            },
        };

        let semaphore = self.pid_report.as_raw_handle() as _;
        let wait_status = if let Some(launcher) = launcher {
            let handles = [semaphore, launcher as _];
            unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout_ms) }
        } else {
            unsafe { WaitForSingleObject(semaphore, timeout_ms) }
        };
        match (wait_status, launcher) {
            (WAIT_OBJECT_0, _) => {}
            (status, Some(launcher)) if status == WAIT_OBJECT_0 + 1 => {
                let mut exit_code = 0_u32;
                if unsafe { GetExitCodeProcess(launcher as _, &mut exit_code) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("the launcher process exited before reporting a PID");
                }
                bail!("the launcher process exited with code {exit_code} before reporting a PID");
            }
            (WAIT_TIMEOUT, _) => {
                bail!("timed out after {timeout_ms} ms waiting for the Windows Grok process ID")
            }
            (WAIT_FAILED, _) => {
                return Err(std::io::Error::last_os_error())
                    .context("failed while waiting for the Windows Grok process ID");
            }
            (status, _) => bail!("unexpected Windows Grok PID wait status: {status}"),
        }
        let mut previous = 0_i32;
        if unsafe { ReleaseSemaphore(semaphore, 1, &mut previous) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read the Windows Grok process ID");
        }
        u32::try_from(previous)
            .ok()
            .and_then(|value| value.checked_add(1))
            .filter(|pid| *pid != 0)
            .context("the Windows Grok launcher reported an invalid process ID")
    }
}

#[cfg(windows)]
fn windows_wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub(super) fn wait_for_windows_launch_gate(name: &std::ffi::OsStr, timeout_ms: u32) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject},
    };

    let wide_name = windows_wide_null(name);
    let raw = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide_name.as_ptr()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("failed to open the Windows Grok launch event");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    match unsafe {
        WaitForSingleObject(
            std::os::windows::io::AsRawHandle::as_raw_handle(&handle) as _,
            timeout_ms,
        )
    } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => {
            bail!("timed out after {timeout_ms} ms waiting for the Windows Grok launch gate")
        }
        WAIT_FAILED => Err(std::io::Error::last_os_error())
            .context("failed while waiting for the Windows Grok launch event"),
        status => bail!("unexpected Windows Grok launch wait status: {status}"),
    }
}

#[cfg(windows)]
pub(super) fn report_windows_worker_pid(name: &std::ffi::OsStr, pid: u32) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenSemaphoreW, ReleaseSemaphore, SEMAPHORE_MODIFY_STATE,
    };

    let pid = i32::try_from(pid).context("the Windows Grok process ID exceeds i32::MAX")?;
    let wide_name = windows_wide_null(name);
    let raw = unsafe { OpenSemaphoreW(SEMAPHORE_MODIFY_STATE, 0, wide_name.as_ptr()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("failed to open the Windows Grok PID report semaphore");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    if unsafe {
        ReleaseSemaphore(
            std::os::windows::io::AsRawHandle::as_raw_handle(&handle) as _,
            pid,
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(std::io::Error::last_os_error()).context("failed to report the Windows Grok process ID")
    } else {
        Ok(())
    }
}

/// Apply one termination level to a bare Windows process, used when no Job
/// Object scope is retained (e.g. tests). Mirrors `WindowsJob::terminate`:
/// `Hup` and `Term` are wait-only phases and `Kill` force-terminates the
/// process with `TerminateProcess`; a process already gone is success.
#[cfg(windows)]
pub(super) fn terminate_windows_process(pid: u32, level: TerminationLevel) -> std::io::Result<()> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    match level {
        TerminationLevel::None => Ok(()),
        TerminationLevel::Hup | TerminationLevel::Term => {
            // Wait-only phase: no force-kill. A process that already exited is
            // observed by the liveness probe.
            Ok(())
        }
        TerminationLevel::Kill => {
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if handle.is_null() {
                return if process_alive(pid) == ScopeAlive::Gone {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                };
            }
            let result = unsafe { TerminateProcess(handle, 1) };
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            if result != 0 || process_alive(pid) == ScopeAlive::Gone {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }
}

#[cfg(windows)]
pub(super) fn process_alive(pid: u32) -> ScopeAlive {
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return if std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_INVALID_PARAMETER as i32)
        {
            ScopeAlive::Gone
        } else {
            ScopeAlive::Unknown
        };
    }
    let status = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    match status {
        WAIT_OBJECT_0 => ScopeAlive::Gone,
        WAIT_TIMEOUT => ScopeAlive::Alive,
        WAIT_FAILED => ScopeAlive::Unknown,
        _ => ScopeAlive::Unknown,
    }
}
