#[cfg(target_os = "linux")]
use std::io::ErrorKind;

use super::{ScopeAlive, TerminationLevel};

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
pub(in crate::session) fn send_termination_signal(
    pid: u32,
    level: TerminationLevel,
) -> std::io::Result<()> {
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
pub(in crate::session) fn process_scope_alive(pid: u32, stable_verification: bool) -> ScopeAlive {
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
pub(in crate::session) fn stable_process_group_scan_result(
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
