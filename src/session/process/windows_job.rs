use std::sync::Arc;

use super::{ScopeAlive, TerminationLevel};

#[cfg(windows)]
#[derive(Clone)]
pub(in crate::session) struct WindowsJob {
    handle: Arc<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl WindowsJob {
    pub(in crate::session) fn create() -> std::io::Result<Self> {
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

    pub(in crate::session) fn assign_process(
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

    pub(in crate::session) fn process_scope_alive(&self) -> ScopeAlive {
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
    pub(in crate::session) fn terminate(&self, level: TerminationLevel) -> std::io::Result<()> {
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

/// Apply one termination level to a bare Windows process, used when no Job
/// Object scope is retained (e.g. tests). Mirrors `WindowsJob::terminate`:
/// `Hup` and `Term` are wait-only phases and `Kill` force-terminates the
/// process with `TerminateProcess`; a process already gone is success.
#[cfg(windows)]
pub(in crate::session) fn terminate_windows_process(
    pid: u32,
    level: TerminationLevel,
) -> std::io::Result<()> {
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
pub(in crate::session) fn process_alive(pid: u32) -> ScopeAlive {
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
