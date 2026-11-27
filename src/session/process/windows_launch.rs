use std::{
    env,
    ffi::{OsStr, OsString},
    path::Path,
};

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
    let codex_no_daemon = config.worker == WorkerKind::Codex
        && worker::codex_supports_no_daemon(config.bin.as_os_str());
    let worker_cmd = worker::build_worker_command_with_options(
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
        codex_no_daemon,
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
    if let Some(mut desktop) = crate::transport::preferred_default_desktop_wide()
        && let Ok((child_handle, child_pid)) =
            spawn_worker_on_desktop(&program, &arguments, &mut desktop)
    {
        return wait_windows_child_handle(child_handle, child_pid, &pid_report_name, &program);
    }
    spawn_and_wait_default_desktop(&program, &arguments, &pid_report_name)
}

#[cfg(windows)]
fn spawn_and_wait_default_desktop(
    program: &OsStr,
    arguments: &[OsString],
    pid_report_name: &OsStr,
) -> Result<i32> {
    let mut command = std::process::Command::new(program);
    command.args(arguments);
    worker::strip_parent_host_env_std(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start gated worker process {program:?}"))?;
    report_windows_worker_pid(pid_report_name, child.id())?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for gated worker process {program:?}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
fn wait_windows_child_handle(
    child_handle: std::os::windows::io::OwnedHandle,
    child_pid: u32,
    pid_report_name: &OsStr,
    program: &OsStr,
) -> Result<i32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::WAIT_FAILED,
        System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
    };

    report_windows_worker_pid(pid_report_name, child_pid)?;
    let raw = child_handle.as_raw_handle() as _;
    if unsafe { WaitForSingleObject(raw, INFINITE) } == WAIT_FAILED {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to wait for gated worker process {program:?}"));
    }
    let mut exit_code = 1_u32;
    if unsafe { GetExitCodeProcess(raw, &mut exit_code) } == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to read exit code for worker process {program:?}"));
    }
    Ok(exit_code as i32)
}

#[cfg(windows)]
fn append_windows_quoted_arg(out: &mut Vec<u16>, arg: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&unit| unit == b' ' as u16 || unit == b'\t' as u16 || unit == b'"' as u16);
    if !needs_quotes {
        out.extend_from_slice(&units);
        return;
    }
    out.push(b'"' as u16);
    let mut backslashes = 0_usize;
    for unit in units {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            backslashes = 0;
            out.push(b'"' as u16);
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            backslashes = 0;
            out.push(unit);
        }
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

#[cfg(windows)]
fn build_windows_command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    append_windows_quoted_arg(&mut out, program);
    for arg in args {
        out.push(b' ' as u16);
        append_windows_quoted_arg(&mut out, arg.as_os_str());
    }
    out.push(0);
    out
}

#[cfg(windows)]
fn spawn_worker_on_desktop(
    program: &OsStr,
    args: &[OsString],
    desktop: &mut [u16],
) -> std::io::Result<(std::os::windows::io::OwnedHandle, u32)> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW},
    };

    worker::strip_parent_host_env_current_process();
    let mut cmd_line = build_windows_command_line(program, args);
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    startup.lpDesktop = desktop.as_mut_ptr();
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(process.hThread);
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess as _) };
    Ok((handle, process.dwProcessId))
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

#[cfg(all(windows, test))]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        os::windows::io::AsRawHandle,
    };

    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };

    use super::{build_windows_command_line, spawn_worker_on_desktop};

    fn decode_cmd(units: &[u16]) -> String {
        let trimmed = units.strip_suffix(&[0]).unwrap_or(units);
        String::from_utf16_lossy(trimmed)
    }

    #[test]
    fn builds_quoted_windows_command_lines_for_spaces_quotes_and_backslashes() {
        let args = vec![
            OsString::from(""),
            OsString::from("plain"),
            OsString::from("two words"),
            OsString::from(r#"a\"b"#),
            OsString::from(r"trail\"),
            OsString::from(r"trail space\"),
        ];
        let cmd = decode_cmd(&build_windows_command_line(
            OsStr::new(r"C:\Program Files\nodejs\node.exe"),
            &args,
        ));
        assert_eq!(
            cmd,
            r#""C:\Program Files\nodejs\node.exe" "" plain "two words" "a\\\"b" trail\ "trail space\\""#
        );
    }

    #[test]
    fn spawns_worker_process_on_default_desktop() {
        let mut desktop: Vec<u16> = "WinSta0\\Default\0".encode_utf16().collect();
        let args = vec![
            OsString::from("/d"),
            OsString::from("/c"),
            OsString::from("exit"),
            OsString::from("0"),
        ];
        let (handle, pid) = spawn_worker_on_desktop(OsStr::new("cmd.exe"), &args, &mut desktop)
            .expect("spawn cmd.exe on WinSta0\\Default");
        assert_ne!(pid, 0);
        let wait = unsafe { WaitForSingleObject(handle.as_raw_handle() as _, INFINITE) };
        assert_eq!(wait, 0);
        let mut code = 1_u32;
        assert_ne!(
            unsafe { GetExitCodeProcess(handle.as_raw_handle() as _, &mut code) },
            0
        );
        assert_eq!(code, 0);
    }
}
