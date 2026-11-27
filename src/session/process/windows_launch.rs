use std::{env, ffi::OsString, path::Path};

use anyhow::{Context, Result, bail};

use super::{
    super::{
        CommandBuilder, LaunchConfig, WINDOWS_LAUNCH_HANDSHAKE_TIMEOUT_MS, WorkerKind,
        generate_provider_session_id, worker,
    },
    WindowsJob,
};

#[cfg(windows)]
pub(in crate::session) struct WindowsLaunchGate {
    pub(in crate::session) event_name: OsString,
    event: std::os::windows::io::OwnedHandle,
    pub(in crate::session) pid_report_name: OsString,
    pid_report: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
pub(in crate::session) fn build_windows_launcher_command(
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
#[cfg(windows)]
impl WindowsLaunchGate {
    pub(in crate::session) fn create() -> Result<Self> {
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

    pub(in crate::session) fn signal(&self) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::SetEvent;

        if unsafe { SetEvent(self.event.as_raw_handle() as _) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(in crate::session) fn wait_for_worker_pid(
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
pub(in crate::session) fn wait_for_windows_launch_gate(
    name: &std::ffi::OsStr,
    timeout_ms: u32,
) -> Result<()> {
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
pub(in crate::session) fn report_windows_worker_pid(
    name: &std::ffi::OsStr,
    pid: u32,
) -> Result<()> {
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
