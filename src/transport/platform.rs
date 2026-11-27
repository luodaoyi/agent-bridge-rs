#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::{env, ffi::OsString};
#[cfg(unix)]
use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{self, ErrorKind},
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use anyhow::bail;
use anyhow::{Context, Result};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, Listener, ListenerOptions};
use interprocess::local_socket::{GenericNamespaced, Name, prelude::*};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ACCESS_DENIED},
    System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW,
        PROCESS_INFORMATION, STARTUPINFOW,
    },
};

#[cfg(unix)]
const RUNTIME_STARTUP_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The Runtime IPC name as understood by the local socket API. On Unix this is
/// a filesystem socket path inside the private owner-only runtime directory; on
/// Windows (and other platforms) it remains a namespaced named pipe. Server and
/// client both compute the name through this function, so the two sides always
/// agree within the same environment.
#[cfg(unix)]
pub(crate) fn runtime_name() -> Result<Name<'static>> {
    runtime_socket_path()?
        .to_fs_name::<GenericFilePath>()
        .context("failed to construct the runtime socket path")
}

/// The legacy Runtime IPC name, kept for upgrade compatibility on Unix.
/// Binaries before the filesystem-socket move bound a GenericNamespaced pipe;
/// a Runtime still running from an older build answers on this name, so the
/// client probes it before starting a new Runtime. This preserves access to
/// existing sessions and avoids a second Runtime contending for the WebUI
/// port. Windows has no migration: the namespaced pipe never changed.
#[cfg(unix)]
pub(crate) fn legacy_runtime_name() -> Result<Name<'static>> {
    runtime_identity()
        .to_ns_name::<GenericNamespaced>()
        .context("failed to construct the legacy runtime pipe name")
}

#[cfg(not(unix))]
pub(crate) fn runtime_name() -> Result<Name<'static>> {
    let identity = runtime_identity();
    identity
        .to_ns_name::<GenericNamespaced>()
        .context("failed to construct the runtime pipe name")
}

/// On Unix, the private per-user directory that holds the Runtime IPC socket.
/// A configured XDG base is used only when it is an absolute, owner-only real
/// directory owned by this uid. Invalid values fall back to an absolute temp
/// base; the private child remains uid-namespaced and is secured to 0700.
#[cfg(unix)]
pub(crate) fn runtime_dir() -> Result<PathBuf> {
    runtime_dir_from(
        env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()),
        env::temp_dir(),
        current_uid(),
    )
}

/// Conservative cross-platform byte budget for a Unix socket address path,
/// excluding the NUL terminator. Linux `sockaddr_un.sun_path` holds 108
/// bytes (107 usable) and macOS holds 104 (103 usable); 100 leaves headroom
/// on both while keeping the path plus its NUL under every supported limit.
#[cfg(unix)]
pub(super) const UNIX_SOCKET_PATH_MAX_BYTES: usize = 100;

/// Raw byte length of a path as it will be stored in a `sockaddr_un.sun_path`.
#[cfg(unix)]
pub(super) fn socket_path_bytes(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}

/// Whether the runtime socket at `base/identity/runtime.sock` fits the Unix
/// socket address limit. An arbitrarily deep XDG_RUNTIME_DIR must never push
/// the socket path past the platform `sockaddr_un` budget, so over-long bases
/// are skipped in favor of the next trusted base.
#[cfg(unix)]
fn socket_path_fits(base: &Path, identity: &OsStr, max_bytes: usize) -> bool {
    socket_path_bytes(&base.join(identity).join("runtime.sock")) <= max_bytes
}

/// Select the runtime base directory. Candidates are tried in order (XDG,
/// temp, `/tmp`); each must pass the existing trust checks and keep the final
/// socket path within `max_bytes`. `/tmp` is always short enough to fit, so
/// the fallback chain is deterministic: the same environment always selects
/// the same base, and per-user isolation (the uid-suffixed identity child),
/// the 0700 directory tightening, and the allowed-roots trust checks are all
/// preserved by [`trusted_runtime_base`].
#[cfg(unix)]
pub(super) fn runtime_dir_from(
    xdg: Option<OsString>,
    temp: PathBuf,
    expected_uid: u32,
) -> Result<PathBuf> {
    runtime_dir_from_with_limit(xdg, temp, expected_uid, UNIX_SOCKET_PATH_MAX_BYTES)
}

#[cfg(unix)]
pub(super) fn runtime_dir_from_with_limit(
    xdg: Option<OsString>,
    temp: PathBuf,
    expected_uid: u32,
    max_socket_bytes: usize,
) -> Result<PathBuf> {
    let identity = runtime_identity();
    let fits = |base: &Path| socket_path_fits(base, &identity, max_socket_bytes);
    let base = xdg
        .map(PathBuf::from)
        .and_then(|path| trusted_runtime_base(&path, expected_uid, true))
        .filter(|base| fits(base))
        .or_else(|| trusted_runtime_base(&temp, expected_uid, false))
        .filter(|base| fits(base))
        .or_else(|| trusted_runtime_base(Path::new("/tmp"), expected_uid, false))
        .filter(|base| fits(base))
        .context("no trusted absolute runtime base fits the Unix socket path limit")?;
    Ok(base.join(identity))
}

#[cfg(all(unix, test))]
pub(super) fn trusted_xdg_runtime_base(path: &Path, expected_uid: u32) -> bool {
    trusted_runtime_base(path, expected_uid, true).is_some()
}

/// Resolve an existing base and verify that no directory in its canonical
/// ancestor chain can be replaced by another user. Root and the current uid
/// are trusted owners. A group/world-writable directory is accepted only with
/// the sticky bit, which prevents peers from renaming another owner's entry.
#[cfg(unix)]
fn trusted_runtime_base(
    path: &Path,
    expected_uid: u32,
    require_owner_only: bool,
) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let original = std::fs::symlink_metadata(path).ok()?;
    if require_owner_only && (original.file_type().is_symlink() || !original.is_dir()) {
        return None;
    }
    if !require_owner_only && !original.is_dir() && !original.file_type().is_symlink() {
        return None;
    }
    if require_owner_only
        && (original.uid() != expected_uid || original.permissions().mode() & 0o077 != 0)
    {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_absolute() {
        return None;
    }
    if !trusted_ancestor_chain(path, expected_uid, true)
        || !trusted_ancestor_chain(&canonical, expected_uid, false)
    {
        return None;
    }
    Some(canonical)
}

#[cfg(unix)]
fn trusted_ancestor_chain(path: &Path, expected_uid: u32, allow_symlinks: bool) -> bool {
    for ancestor in path.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(ancestor) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            if !allow_symlinks || (meta.uid() != 0 && meta.uid() != expected_uid) {
                return false;
            }
            continue;
        }
        if !meta.is_dir() {
            return false;
        }
        if meta.uid() != 0 && meta.uid() != expected_uid {
            return false;
        }
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return false;
        }
    }
    true
}

/// On Unix, the socket file lives inside the private runtime directory.
#[cfg(unix)]
pub(crate) fn runtime_socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("runtime.sock"))
}

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
pub(super) fn start_detached_server() -> Result<()> {
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
pub(super) fn start_detached_server() -> Result<()> {
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
pub(super) fn start_detached_server() -> Result<()> {
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

#[cfg(windows)]
pub(super) fn runtime_identity() -> OsString {
    let user = env::var("USERNAME").unwrap_or_else(|_| "default".to_owned());
    let domain = env::var("USERDOMAIN").unwrap_or_default();
    let suffix = format!("{domain}-{user}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    OsString::from(format!("agent-bridge-runtime-v1-{suffix}"))
}

#[cfg(unix)]
pub(crate) fn runtime_identity() -> OsString {
    let uid = unsafe { libc::getuid() };
    OsString::from(format!("agent-bridge-runtime-v1-u{uid}"))
}

#[cfg(not(any(windows, unix)))]
pub(super) fn runtime_identity() -> OsString {
    OsString::from("agent-bridge-runtime-v1-default")
}

/// Owner-only mode for the Runtime IPC directory on Unix.
#[cfg(unix)]
pub(crate) const RUNTIME_DIR_MODE: u32 = 0o700;
/// Owner-only mode for the Runtime IPC socket on Unix. Unix sockets authorize
/// connection attempts with the write bits of the socket file, so 0600 admits
/// only the owner.
#[cfg(unix)]
pub(crate) const RUNTIME_SOCKET_MODE: u32 = 0o600;

#[cfg(unix)]
pub(super) fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Holds the advisory startup lock while one Runtime binds, probes, removes a
/// stale socket, and retries the bind. Closing the file releases the lock.
#[cfg(unix)]
pub(crate) struct RuntimeStartupLock {
    _file: File,
}

#[cfg(unix)]
pub(crate) fn acquire_runtime_startup_lock() -> Result<RuntimeStartupLock> {
    let dir = ensure_runtime_dir()?;
    acquire_runtime_startup_lock_at(&dir, RUNTIME_STARTUP_LOCK_TIMEOUT)
}

#[cfg(unix)]
pub(super) fn acquire_runtime_startup_lock_at(
    dir: &Path,
    timeout: Duration,
) -> Result<RuntimeStartupLock> {
    let path = dir.join("runtime.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(RUNTIME_SOCKET_MODE)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("failed to open the Runtime startup lock {path:?}"))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to inspect the Runtime startup lock {path:?}"))?;
    if !meta.is_file() || meta.uid() != current_uid() {
        bail!(
            "Runtime startup lock {path:?} is not a regular file owned by uid {}",
            current_uid()
        );
    }
    file.set_permissions(std::fs::Permissions::from_mode(RUNTIME_SOCKET_MODE))
        .with_context(|| format!("failed to secure the Runtime startup lock {path:?}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(RuntimeStartupLock { _file: file });
        }
        let error = io::Error::last_os_error();
        let raw_error = error.raw_os_error();
        if raw_error != Some(libc::EWOULDBLOCK) && raw_error != Some(libc::EAGAIN) {
            return Err(error)
                .with_context(|| format!("failed to lock Runtime startup file {path:?}"));
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the Runtime startup lock {path:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Create or verify the private Runtime IPC directory. The directory must be a
/// real directory owned by the current user; a symlink, another file type, or a
/// foreign owner is refused with a diagnosable error instead of being touched.
/// An owned directory is then tightened to owner-only (0700) and the resulting
/// mode is verified on disk.
#[cfg(unix)]
pub(crate) fn ensure_runtime_dir() -> Result<PathBuf> {
    let dir = runtime_dir()?;
    ensure_private_dir(&dir, current_uid())
        .with_context(|| format!("failed to secure the runtime IPC directory {dir:?}"))?;
    Ok(dir)
}

/// Verify that `path` is a directory owned by `expected_uid` and make it
/// owner-only. Uses `symlink_metadata` (lstat) so a symlink at the path is
/// rejected instead of followed. `expected_uid` is a parameter so tests can
/// exercise the ownership check without impersonating another user.
#[cfg(unix)]
pub(super) fn ensure_private_dir(path: &Path, expected_uid: u32) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!(
                    "{path:?} is a symlink; refusing to use or modify it — remove the link manually"
                );
            }
            if !meta.is_dir() {
                bail!("{path:?} is {:?}, not a directory", meta.file_type());
            }
            if meta.uid() != expected_uid {
                bail!(
                    "{path:?} is owned by uid {} (current user uid {}); refusing to use it",
                    meta.uid(),
                    expected_uid
                );
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(RUNTIME_DIR_MODE);
            builder
                .create(path)
                .with_context(|| format!("failed to create the runtime IPC directory {path:?}"))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {path:?}"));
        }
    }
    set_owner_only_mode(path)
}

/// Apply mode 0700 to a directory already verified as ours and confirm the mode
/// on disk. Chmod by path is safe here: only the owner of a directory can
/// change its permissions, so no other actor can widen them in between.
#[cfg(unix)]
fn set_owner_only_mode(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(RUNTIME_DIR_MODE))
        .with_context(|| format!("failed to set owner-only permissions on {path:?}"))?;
    let mode = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to re-inspect {path:?} after chmod"))?
        .permissions()
        .mode()
        & 0o777;
    if mode != RUNTIME_DIR_MODE {
        bail!("{path:?} has mode {mode:o} after chmod; expected owner-only {RUNTIME_DIR_MODE:o}");
    }
    Ok(())
}

/// Whether the path currently holds a stale socket that may be removed.
/// Returns `Ok(true)` when the path is a socket file owned by `expected_uid`,
/// and `Ok(false)` when nothing is there. A symlink, any other file type, or a
/// foreign owner is refused with a diagnosable error; the path is never
/// followed or removed in those cases.
#[cfg(unix)]
pub(super) fn stale_socket_safe_to_remove(path: &Path, expected_uid: u32) -> Result<bool> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("failed to inspect {path:?}")),
    };
    if meta.file_type().is_symlink() {
        bail!("{path:?} is a symlink; refusing to remove it — delete the link manually and re-run");
    }
    if !meta.file_type().is_socket() {
        bail!(
            "{path:?} is {:?}, not a Unix domain socket; refusing to remove it",
            meta.file_type()
        );
    }
    if meta.uid() != expected_uid {
        bail!(
            "{path:?} is owned by uid {} (current user uid {}); refusing to remove it",
            meta.uid(),
            expected_uid
        );
    }
    Ok(true)
}

/// Remove a stale Runtime socket, but only when it is provably our own socket
/// file: never a symlink, never another file type, never a foreign owner.
/// `Ok(false)` means the path was already gone. Unsafe paths surface as
/// diagnosable errors instead of being deleted.
#[cfg(unix)]
pub(super) fn remove_stale_socket_at(path: &Path) -> Result<bool> {
    if !stale_socket_safe_to_remove(path, current_uid())? {
        return Ok(false);
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to remove the stale runtime socket {path:?}"))?;
    Ok(true)
}

/// Remove the stale socket at the current Runtime IPC path, if one exists and
/// is safe to remove. Called after a bind failure when no live Runtime
/// answered, so a crashed server's leftover socket can be reclaimed.
#[cfg(unix)]
pub(crate) fn remove_stale_runtime_socket(_lock: &RuntimeStartupLock) -> Result<bool> {
    remove_stale_socket_at(&runtime_socket_path()?)
}

/// Apply owner-only (0600) permissions to a freshly bound Runtime socket and
/// verify its on-disk state: a socket file owned by the current user with
/// exactly mode 0600. Chmod by path is safe because the socket lives inside the
/// 0700 owner-only runtime directory, so no other user can replace the file
/// between our syscalls.
#[cfg(unix)]
pub(crate) fn verify_runtime_socket(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect the runtime socket {path:?}"))?;
    if meta.file_type().is_symlink() {
        bail!("the runtime socket {path:?} is a symlink; refusing to serve it");
    }
    if !meta.file_type().is_socket() {
        bail!(
            "the runtime socket {path:?} is {:?}, not a Unix domain socket",
            meta.file_type()
        );
    }
    if meta.uid() != current_uid() {
        bail!(
            "the runtime socket {path:?} is owned by uid {} (current user uid {}); refusing to serve it",
            meta.uid(),
            current_uid()
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != RUNTIME_SOCKET_MODE {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(RUNTIME_SOCKET_MODE))
            .with_context(|| {
                format!("failed to set owner-only permissions on the runtime socket {path:?}")
            })?;
        let mode = std::fs::symlink_metadata(path)
            .with_context(|| {
                format!("failed to re-inspect the runtime socket {path:?} after chmod")
            })?
            .permissions()
            .mode()
            & 0o777;
        if mode != RUNTIME_SOCKET_MODE {
            bail!(
                "the runtime socket {path:?} has mode {mode:o} after chmod; expected owner-only {RUNTIME_SOCKET_MODE:o}"
            );
        }
    }
    Ok(())
}

/// Bind a Unix filesystem local socket listener with owner-only permissions.
/// The socket mode is set through interprocess (`fchmod` before `bind`, immune
/// to umask races) when the platform supports it; otherwise the listener is
/// bound first and secured by path afterwards — safe because the socket is
/// created inside the 0700 owner-only runtime directory, which no other user
/// can traverse or modify. The resulting on-disk mode is always verified.
#[cfg(unix)]
pub(crate) fn bind_listener_at(path: &Path) -> io::Result<Listener> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt;
    let name = path.to_fs_name::<GenericFilePath>()?;
    let secure = |listener: Listener| -> io::Result<Listener> {
        verify_runtime_socket(path).map_err(|error| io::Error::other(format!("{error:#}")))?;
        Ok(listener)
    };
    match ListenerOptions::new()
        .name(name.clone())
        .mode(RUNTIME_SOCKET_MODE as libc::mode_t)
        .create_sync()
    {
        Ok(listener) => secure(listener),
        Err(error) if error.kind() == ErrorKind::Unsupported => {
            // The platform cannot fchmod sockets (e.g. macOS): bind first, then
            // apply the mode by path and verify.
            let listener = ListenerOptions::new().name(name).create_sync()?;
            secure(listener)
        }
        Err(error) => Err(error),
    }
}
