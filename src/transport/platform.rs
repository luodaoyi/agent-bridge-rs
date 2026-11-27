#[cfg(windows)]
use std::env;
use std::ffi::OsString;

use anyhow::{Context, Result};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::{GenericNamespaced, Name, prelude::*};

#[cfg(windows)]
mod desktop;
#[cfg(unix)]
mod runtime_dir;
mod spawn;
#[cfg(unix)]
mod unix_socket;

#[cfg(windows)]
pub(crate) use desktop::preferred_default_desktop_wide;
#[cfg(unix)]
pub(crate) use runtime_dir::{runtime_dir, runtime_socket_path};
pub(super) use spawn::start_detached_server;
#[cfg(unix)]
pub(crate) use unix_socket::{
    RUNTIME_DIR_MODE, RUNTIME_SOCKET_MODE, RuntimeStartupLock, acquire_runtime_startup_lock,
    bind_listener_at, ensure_runtime_dir, remove_stale_runtime_socket, verify_runtime_socket,
};

#[cfg(all(unix, test))]
pub(super) use runtime_dir::{
    UNIX_SOCKET_PATH_MAX_BYTES, runtime_dir_from, runtime_dir_from_with_limit, socket_path_bytes,
    trusted_xdg_runtime_base,
};
#[cfg(all(unix, test))]
pub(super) use unix_socket::{
    acquire_runtime_startup_lock_at, current_uid, ensure_private_dir, remove_stale_socket_at,
    stale_socket_safe_to_remove,
};

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
