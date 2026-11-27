#[cfg(unix)]
use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

#[cfg(unix)]
use anyhow::{Context, Result};

#[cfg(unix)]
use super::{runtime_identity, unix_socket::current_uid};

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
pub(in crate::transport) const UNIX_SOCKET_PATH_MAX_BYTES: usize = 100;

/// Raw byte length of a path as it will be stored in a `sockaddr_un.sun_path`.
#[cfg(unix)]
pub(in crate::transport) fn socket_path_bytes(path: &Path) -> usize {
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
pub(in crate::transport) fn runtime_dir_from(
    xdg: Option<OsString>,
    temp: PathBuf,
    expected_uid: u32,
) -> Result<PathBuf> {
    runtime_dir_from_with_limit(xdg, temp, expected_uid, UNIX_SOCKET_PATH_MAX_BYTES)
}

#[cfg(unix)]
pub(in crate::transport) fn runtime_dir_from_with_limit(
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
pub(in crate::transport) fn trusted_xdg_runtime_base(path: &Path, expected_uid: u32) -> bool {
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
