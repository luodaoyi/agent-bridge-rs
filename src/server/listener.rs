use std::io;
#[cfg(unix)]
use std::io::ErrorKind;

use anyhow::{Context, Result};
#[cfg(not(unix))]
use interprocess::local_socket::ListenerOptions;
#[cfg(unix)]
use interprocess::local_socket::Name;
use interprocess::local_socket::{Listener, Stream, prelude::*};

use crate::transport::runtime_name;
#[cfg(not(unix))]
use crate::{
    protocol::{Request, ResponseResult},
    transport::call_anonymous,
};

/// Bind the Runtime IPC listener. On Unix this secures a filesystem socket to
/// owner-only (0600) inside the private owner-only runtime directory; on other
/// platforms it keeps the historical named-pipe bind. Returns the raw I/O
/// error so `run` can distinguish an occupied name from other bind failures.
pub(super) fn bind_runtime_listener() -> io::Result<Listener> {
    #[cfg(unix)]
    {
        let path = crate::transport::runtime_socket_path().map_err(io::Error::other)?;
        crate::transport::bind_listener_at(&path)
    }
    #[cfg(not(unix))]
    {
        let name = runtime_name().map_err(io::Error::other)?;
        ListenerOptions::new().name(name).create_sync()
    }
}

/// Whether another Runtime owns the IPC name. On Unix this probes the socket
/// directly: a live listener accepts connections even while at its connection
/// cap, whereas a stale socket file refuses them. Other platforms keep the
/// historical `ServerStatus` round-trip.
pub(super) fn runtime_server_is_alive() -> bool {
    #[cfg(unix)]
    {
        runtime_name()
            .map(|name| runtime_server_is_alive_for(&name))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        call_anonymous(Request::ServerStatus, false).is_ok_and(|response| {
            response.ok && matches!(response.result, Some(ResponseResult::ServerInfo(_)))
        })
    }
}

/// Whether a Runtime started by an older binary still answers on the legacy
/// GenericNamespaced endpoint (Unix only; other platforms share one endpoint
/// between old and new binaries, so the normal liveness probe covers them).
/// The legacy name never collides with the new filesystem socket, so without
/// this probe a duplicate server could bind the new socket and create a second
/// Runtime. Same connect-based semantics as [`runtime_server_is_alive_for`]:
/// a live listener is active, a missing or refused name is absent, and any
/// other connect error is surfaced as `Uncertain` so startup can report why it
/// refused to start rather than mislabeling a live Runtime as a stale socket.
#[cfg(unix)]
pub(super) fn legacy_runtime_server_probe() -> LegacyProbe {
    match crate::transport::legacy_runtime_name() {
        Ok(name) => match probe_runtime_name(&name) {
            ProbeOutcome::Live => LegacyProbe::Active,
            ProbeOutcome::Dead => LegacyProbe::Absent,
            ProbeOutcome::Uncertain(error) => LegacyProbe::Uncertain(error),
        },
        // An unconstructible legacy name cannot be probed; treat it as absent
        // so startup proceeds to bind the filesystem socket.
        Err(_) => LegacyProbe::Absent,
    }
}

/// Outcome of probing the legacy endpoint. Mirrors [`runtime_server_is_alive_for`]
/// except that an inconclusive probe error is retained instead of being
/// collapsed into a boolean.
#[cfg(unix)]
pub(super) enum LegacyProbe {
    /// A live listener accepted the connection: an older Runtime is running.
    Active,
    /// Definitively no older Runtime (missing or refused endpoint).
    Absent,
    /// The probe could not conclude; `error` is the original connect error.
    Uncertain(io::Error),
}

/// Unix liveness probe for a single name: a live listener accepts connections,
/// while a stale socket file refuses them. ENOENT (no socket file) and
/// ECONNREFUSED (a socket file with nothing listening behind it) are the only
/// definitive "dead" signals. Any other connect error — EACCES, EMFILE,
/// transient failures — is reported as `Uncertain` with the original error
/// instead of being collapsed into a boolean. Never removes anything.
#[cfg(unix)]
pub(super) fn probe_runtime_name(name: &Name<'_>) -> ProbeOutcome {
    match Stream::connect(name.clone()) {
        Ok(_) => ProbeOutcome::Live,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            ProbeOutcome::Dead
        }
        Err(error) => ProbeOutcome::Uncertain(error),
    }
}

/// Outcome of a single-name Unix liveness probe.
#[cfg(unix)]
#[derive(Debug)]
pub(super) enum ProbeOutcome {
    Live,
    Dead,
    Uncertain(io::Error),
}

/// Conservative bool for the current-endpoint probe: any outcome other than a
/// definitive "dead" must count as alive, or a live Runtime's socket could be
/// removed. This is the semantic [`rebind_after_stale_socket`] relies on.
#[cfg(unix)]
pub(super) fn runtime_server_is_alive_for(name: &Name<'_>) -> bool {
    !matches!(probe_runtime_name(name), ProbeOutcome::Dead)
}

/// The IPC name is held by a process that did not answer the liveness probe.
/// On Unix this is typically a stale socket file left by a crashed Runtime:
/// remove it only when it is provably our own socket file, then retry the bind
/// once. Unsafe paths — symlinks, wrong file types, foreign owners — are
/// refused with a diagnosable error instead of being deleted. Other platforms
/// keep the historical "occupied" error.
pub(super) fn rebind_after_stale_socket(
    error: io::Error,
    #[cfg(unix)] startup_lock: &crate::transport::RuntimeStartupLock,
) -> Result<Listener> {
    #[cfg(unix)]
    {
        // Re-probe under the startup lock before removing anything: a live
        // Runtime may have claimed the name between the first probe and now,
        // and deleting its socket would break the running server.
        if runtime_server_is_alive() {
            return Err(error).context("runtime socket path is occupied by another process");
        }
        if crate::transport::remove_stale_runtime_socket(startup_lock)? {
            match bind_runtime_listener() {
                Ok(listener) => Ok(listener),
                Err(retry_error) => Err(retry_error)
                    .context("runtime socket was re-occupied after removing the stale socket"),
            }
        } else {
            Err(error).context("runtime socket path is occupied by another process")
        }
    }
    #[cfg(not(unix))]
    {
        Err(error).context("runtime pipe name is occupied by another process")
    }
}

pub(super) fn wake_listener() {
    if let Ok(name) = runtime_name() {
        let _ = Stream::connect(name);
    }
}
