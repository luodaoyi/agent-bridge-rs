mod hook_http;
mod http;
mod ipc;
mod listener;
#[cfg(test)]
mod tests;
mod ws;

#[cfg(test)]
use crate::protocol::decode_write_data;
use std::{
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[cfg(test)]
use std::{
    io::{self, BufReader},
    time::Instant,
};

use anyhow::{Context, Result};
use interprocess::local_socket::prelude::*;

use crate::{
    protocol::{ResponseEnvelope, ServerInfo},
    session::{OrphanPolicy, SessionHost},
    transport::{IPC_REJECT_DEADLINE, write_response_with_deadline},
    version_check::{CHECK_INTERVAL, VersionChecker},
};

#[cfg(unix)]
use self::listener::{LegacyProbe, legacy_runtime_server_probe};
#[cfg(all(test, unix))]
use self::listener::{ProbeOutcome, probe_runtime_name, runtime_server_is_alive_for};
use self::{
    http::{bind_web_ui, run_web_ui},
    ipc::handle_connection,
    listener::{bind_runtime_listener, rebind_after_stale_socket, runtime_server_is_alive},
};

/// Bound WebSocket text/binary payload size (server and client).
const WEB_EVENTS_MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Max idle sleep between client-frame polls so interactive keys stay low-latency.
/// Host Condvar still wakes immediately on session revisions.
const WEB_EVENTS_CLIENT_POLL: Duration = Duration::from_millis(25);
/// Cap inbound request-id length for WebUI command frames.
const WEB_EVENTS_MAX_REQUEST_ID_BYTES: usize = 128;
/// Hard per-connection bound on tracked request ids. This keeps request-id
/// memory within a small per-connection budget; when full, the next new command
/// is rejected with an explicit reconnect result and the connection is closed.
/// Old ids are never evicted or replayable within one connection.
const WEB_EVENTS_MAX_TRACKED_REQUEST_IDS: usize = 8192;

/// Active connection caps: the Runtime never spawns an unbounded number of
/// handler threads for either local listener. Reaching a cap rejects new
/// connections with a bounded error instead of creating another thread.
const WEB_MAX_ACTIVE_CONNECTIONS: usize = 64;
const IPC_MAX_ACTIVE_CONNECTIONS: usize = 64;

/// HTTP parser and I/O bounds for the loopback WebUI.
/// Request line and per-header line byte caps.
const WEB_HTTP_MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const WEB_HTTP_MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
/// Maximum number of header lines per request.
const WEB_HTTP_MAX_HEADER_COUNT: usize = 64;
/// Maximum total header bytes (names, values, CRLFs) per request.
const WEB_HTTP_MAX_HEADER_BYTES: usize = 32 * 1024;
/// Maximum response body the WebUI will serialize onto the wire. Bodies above
/// this are replaced with a bounded 500 error instead of being sent verbatim.
const WEB_HTTP_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Per-read socket timeout: a stalled peer fails one read quickly.
const WEB_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Total wall-clock budget for reading one HTTP request; a trickling client
/// cannot hold a handler thread forever one byte at a time.
const WEB_HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// Socket write timeout for WebUI HTTP and WebSocket writes.
const WEB_HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Short total deadline for a WebUI rejection response written directly from
/// the accept thread: a peer that never drains must not pin acceptance of
/// further connections for the full write timeout.
const WEB_HTTP_REJECT_DEADLINE: Duration = Duration::from_secs(2);
/// Bounded backoff while an HTTP response write is blocked on a full socket
/// buffer, so a stalled peer is retried with sleeps instead of a busy loop.
const WEB_HTTP_WRITE_POLL_MIN: Duration = Duration::from_millis(1);
const WEB_HTTP_WRITE_POLL_MAX: Duration = Duration::from_millis(50);
/// Idle poll interval while waiting for HTTP request bytes.
const WEB_HTTP_POLL: Duration = Duration::from_millis(5);

pub(crate) fn run() -> Result<()> {
    // On Unix one owner-only lock serializes bind, liveness probe, stale
    // cleanup, and rebind. It is released immediately after listener ownership
    // is established, and by the OS if a starter crashes.
    #[cfg(unix)]
    let startup_lock = crate::transport::acquire_runtime_startup_lock()?;
    // Upgrade compatibility: a Runtime started by an older binary still owns
    // the legacy GenericNamespaced endpoint. Binding the new filesystem socket
    // would succeed (different name) and create a second Runtime, stranding old
    // sessions and contending for the WebUI port. Keep the singleton: exit
    // quietly so clients keep using the legacy Runtime until it stops.
    #[cfg(unix)]
    match legacy_runtime_server_probe() {
        LegacyProbe::Active => {
            eprintln!(
                "agent-bridge server: an older Runtime is still running on the legacy endpoint \
                 `{}`; exiting so clients keep using it. Stop that Runtime (close its terminal \
                 or press Ctrl-C, or kill the agent-bridge process) and start this version again.",
                crate::transport::runtime_identity().to_string_lossy()
            );
            return Ok(());
        }
        LegacyProbe::Uncertain(error) => {
            eprintln!(
                "agent-bridge server: cannot determine whether an older Runtime still owns the \
                 legacy endpoint `{}` (probe failed: {error}); exiting rather than risk a second \
                 Runtime. Stop the older Runtime, or if none is running remove or repair the \
                 stale legacy endpoint, then start this version again.",
                crate::transport::runtime_identity().to_string_lossy()
            );
            return Ok(());
        }
        LegacyProbe::Absent => {}
    }
    let listener = match bind_runtime_listener() {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::AddrInUse | ErrorKind::PermissionDenied
            ) =>
        {
            if runtime_server_is_alive() {
                // Another Runtime already owns the IPC name; the singleton is
                // running, so this duplicate server exits quietly.
                return Ok(());
            }
            rebind_after_stale_socket(
                error,
                #[cfg(unix)]
                &startup_lock,
            )?
        }
        Err(error) => {
            #[cfg(not(unix))]
            return Err(error).context("failed to bind the runtime named pipe");
            #[cfg(unix)]
            return Err(error).context("failed to bind the runtime IPC listener");
        }
    };
    #[cfg(unix)]
    drop(startup_lock);

    let web_listener = bind_web_ui()?;
    let hook_token = hook_http::generate_hook_token()?;
    let _hook_endpoint_guard = if let Some(ref listener) = web_listener
        && let Ok(address) = listener.local_addr()
    {
        match hook_http::HookEndpointGuard::publish(address, &hook_token) {
            Ok(guard) => Some(guard),
            Err(error) => {
                eprintln!("agent-bridge server: hook HTTP endpoint unavailable: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let web_url = web_listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| format!("http://{address}/"));
    let state = Arc::new(RuntimeState {
        host: SessionHost::new(OrphanPolicy::from_env()?),
        started_at_ms: now_millis(),
        stopping: AtomicBool::new(false),
        web_url,
        hook_token,
        version_checker: Arc::new(VersionChecker::new()),
        web_connections: AtomicUsize::new(0),
        ipc_connections: AtomicUsize::new(0),
    });
    if let Some(listener) = web_listener {
        let web_state = Arc::clone(&state);
        thread::spawn(move || run_web_ui(listener, web_state));
    }
    {
        let reaper_state = Arc::clone(&state);
        thread::spawn(move || run_orphan_reaper(reaper_state));
    }
    {
        let version_state = Arc::clone(&state);
        thread::spawn(move || run_version_checker(version_state));
    }

    for connection in listener.incoming() {
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                if state.stopping.load(Ordering::Acquire) {
                    break;
                }
                eprintln!("agent-bridge server: failed to accept client: {error}");
                continue;
            }
        };
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        if !admit_connection(&state.ipc_connections, IPC_MAX_ACTIVE_CONNECTIONS) {
            // At the IPC connection cap: answer with a bounded error frame
            // under a short deadline and close without spawning another
            // handler thread. The short deadline keeps a peer that never
            // drains from pinning the accept loop.
            let nonblocking = connection.set_nonblocking(true).is_ok();
            let busy = ResponseEnvelope::failure(
                "invalid-request",
                "server_busy",
                "the runtime is at its IPC connection limit; retry shortly",
            );
            let mut connection = connection;
            if nonblocking {
                let _ = write_response_with_deadline(&mut connection, &busy, IPC_REJECT_DEADLINE);
            }
            continue;
        }
        let state = Arc::clone(&state);
        thread::spawn(move || {
            // Keep a dedicated Arc for the slot so the counter stays alive for
            // the whole handler while `state` moves into the handler.
            let slot_state = Arc::clone(&state);
            let _slot = ConnectionSlot {
                slots: &slot_state.ipc_connections,
            };
            handle_connection(connection, state);
        });
    }

    state.host.shutdown_all()?;
    Ok(())
}

struct RuntimeState {
    host: SessionHost,
    started_at_ms: u64,
    stopping: AtomicBool,
    web_url: Option<String>,
    hook_token: String,
    version_checker: Arc<VersionChecker>,
    /// Active WebUI HTTP/WebSocket handler threads.
    web_connections: AtomicUsize,
    /// Active IPC request handler threads.
    ipc_connections: AtomicUsize,
}

/// Atomically reserve one of `limit` handler slots; `false` means the cap is
/// reached and the caller must reject the connection without spawning a thread.
fn admit_connection(slots: &AtomicUsize, limit: usize) -> bool {
    let mut current = slots.load(Ordering::Acquire);
    while current < limit {
        match slots.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
    false
}

/// Releases a reserved connection slot when the handler thread finishes.
struct ConnectionSlot<'a> {
    slots: &'a AtomicUsize,
}

impl Drop for ConnectionSlot<'_> {
    fn drop(&mut self) {
        self.slots.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run_orphan_reaper(state: Arc<RuntimeState>) {
    while !state.stopping.load(Ordering::Acquire) {
        thread::sleep(Duration::from_secs(5));
        if state.stopping.load(Ordering::Acquire) {
            return;
        }
        // Only real close/removal paths notify the WebUI revision bus.
        if let Err(error) = state.host.reap_orphans() {
            eprintln!("agent-bridge server: orphan cleanup failed: {error:#}");
        }
    }
}

fn run_version_checker(state: Arc<RuntimeState>) {
    loop {
        if state.stopping.load(Ordering::Acquire) {
            return;
        }
        state.version_checker.refresh();
        let mut remaining = CHECK_INTERVAL;
        while remaining > Duration::ZERO {
            if state.stopping.load(Ordering::Acquire) {
                return;
            }
            let slice = remaining.min(Duration::from_secs(30));
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
    }
}

impl RuntimeState {
    fn server_info(&self) -> ServerInfo {
        ServerInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            process_id: std::process::id(),
            started_at_ms: self.started_at_ms,
            active_sessions: self.host.active_count(),
            web_url: self.web_url.clone(),
            stopping: self.stopping.load(Ordering::Acquire),
            hook_diagnostics: Some(self.host.hook_diagnostics()),
        }
    }
}

const WEB_UI_HTML: &[u8] = include_bytes!("../webui/dist/index.html");
const WEB_UI_JS: &[u8] = include_bytes!("../webui/dist/assets/app.js");
const WEB_UI_CSS: &[u8] = include_bytes!("../webui/dist/assets/app.css");

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
