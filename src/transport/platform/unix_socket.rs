#[cfg(unix)]
use std::{
    fs::{File, OpenOptions},
    io::{self, ErrorKind},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, Listener, ListenerOptions, prelude::*};

#[cfg(unix)]
use super::{runtime_dir, runtime_socket_path};

#[cfg(unix)]
const RUNTIME_STARTUP_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Owner-only mode for the Runtime IPC directory on Unix.
#[cfg(unix)]
pub(crate) const RUNTIME_DIR_MODE: u32 = 0o700;
/// Owner-only mode for the Runtime IPC socket on Unix. Unix sockets authorize
/// connection attempts with the write bits of the socket file, so 0600 admits
/// only the owner.
#[cfg(unix)]
pub(crate) const RUNTIME_SOCKET_MODE: u32 = 0o600;

#[cfg(unix)]
pub(in crate::transport) fn current_uid() -> u32 {
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
pub(in crate::transport) fn acquire_runtime_startup_lock_at(
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
pub(in crate::transport) fn ensure_private_dir(path: &Path, expected_uid: u32) -> Result<()> {
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
pub(in crate::transport) fn stale_socket_safe_to_remove(
    path: &Path,
    expected_uid: u32,
) -> Result<bool> {
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
pub(in crate::transport) fn remove_stale_socket_at(path: &Path) -> Result<bool> {
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
