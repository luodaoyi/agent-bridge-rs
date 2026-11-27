use std::env;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(not(windows))]
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::{ffi::OsString, os::windows::ffi::OsStrExt};

use anyhow::{Context, Result};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ACCESS_DENIED},
    System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW,
        PROCESS_INFORMATION, STARTUPINFOW,
    },
};

#[cfg(windows)]
fn detached_server_creation_flags() -> [u32; 2] {
    let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    [base | CREATE_BREAKAWAY_FROM_JOB, base]
}

#[cfg(windows)]
fn should_retry_without_breakaway(raw_os_error: Option<i32>) -> bool {
    raw_os_error == Some(ERROR_ACCESS_DENIED as i32)
}

#[cfg(windows)]
pub(in crate::transport) fn start_detached_server() -> Result<()> {
    let executable = env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut application = executable.as_os_str().encode_wide().collect::<Vec<_>>();
    application.push(0);
    let mut command_line_template =
        OsString::from(format!("\"{}\" __server", executable.display()))
            .encode_wide()
            .collect::<Vec<_>>();
    command_line_template.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let flags = detached_server_creation_flags();
    let mut process = PROCESS_INFORMATION::default();
    let mut command_line = command_line_template.clone();
    let mut created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags[0],
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        let error = std::io::Error::last_os_error();
        if should_retry_without_breakaway(error.raw_os_error()) {
            process = PROCESS_INFORMATION::default();
            command_line = command_line_template;
            created = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    flags[1],
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup,
                    &mut process,
                )
            };
        } else {
            return Err(error).context("failed to spawn runtime server");
        }
    }
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to spawn runtime server");
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(unix)]
pub(in crate::transport) fn start_detached_server() -> Result<()> {
    let executable = env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and the callback does not access shared state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().context("failed to spawn runtime server")?;
    Ok(())
}

#[cfg(not(any(windows, unix)))]
pub(in crate::transport) fn start_detached_server() -> Result<()> {
    let executable = env::current_exe().context("failed to locate agent-bridge executable")?;
    Command::new(executable)
        .arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn runtime server")?;
    Ok(())
}
