mod hook_http;
mod http;
#[cfg(test)]
mod tests;
mod ws;

use std::{
    io::{self, BufReader, ErrorKind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
#[cfg(not(unix))]
use interprocess::local_socket::ListenerOptions;
#[cfg(unix)]
use interprocess::local_socket::Name;
use interprocess::local_socket::{Listener, Stream, prelude::*};

#[cfg(windows)]
use crate::transport::RemapNowaitEmptyRead;
#[cfg(not(unix))]
use crate::transport::call_anonymous;
use crate::{
    protocol::{
        Request, ResponseEnvelope, ResponseResult, ServerInfo, decode_request, decode_write_data,
    },
    session::{OrphanPolicy, SessionHost},
    transport::{
        IPC_FRAME_READ_DEADLINE, IPC_REJECT_DEADLINE, read_frame, runtime_name, write_response,
        write_response_with_deadline,
    },
    version_check::{CHECK_INTERVAL, VersionChecker},
};

use self::http::{bind_web_ui, run_web_ui};

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

/// Bind the Runtime IPC listener. On Unix this secures a filesystem socket to
/// owner-only (0600) inside the private owner-only runtime directory; on other
/// platforms it keeps the historical named-pipe bind. Returns the raw I/O
/// error so `run` can distinguish an occupied name from other bind failures.
fn bind_runtime_listener() -> io::Result<Listener> {
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
fn runtime_server_is_alive() -> bool {
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
fn legacy_runtime_server_probe() -> LegacyProbe {
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
enum LegacyProbe {
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
fn probe_runtime_name(name: &Name<'_>) -> ProbeOutcome {
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
enum ProbeOutcome {
    Live,
    Dead,
    Uncertain(io::Error),
}

/// Conservative bool for the current-endpoint probe: any outcome other than a
/// definitive "dead" must count as alive, or a live Runtime's socket could be
/// removed. This is the semantic [`rebind_after_stale_socket`] relies on.
#[cfg(unix)]
fn runtime_server_is_alive_for(name: &Name<'_>) -> bool {
    !matches!(probe_runtime_name(name), ProbeOutcome::Dead)
}

/// The IPC name is held by a process that did not answer the liveness probe.
/// On Unix this is typically a stale socket file left by a crashed Runtime:
/// remove it only when it is provably our own socket file, then retry the bind
/// once. Unsafe paths — symlinks, wrong file types, foreign owners — are
/// refused with a diagnosable error instead of being deleted. Other platforms
/// keep the historical "occupied" error.
fn rebind_after_stale_socket(
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

fn handle_connection(stream: Stream, state: Arc<RuntimeState>) {
    // Non-blocking I/O lets the request read and the response write be bounded
    // by wall-clock deadlines on every platform (Windows named pipes have no
    // native socket timeouts).
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    #[cfg(windows)]
    let mut connection = BufReader::new(RemapNowaitEmptyRead(stream));
    #[cfg(not(windows))]
    let mut connection = BufReader::new(stream);
    let frame = match read_frame(
        &mut connection,
        Some(Instant::now() + IPC_FRAME_READ_DEADLINE),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            // The peer did not deliver a complete request (silent, closed, or
            // oversized). Record the bounded error and close; there is no
            // request to answer and writing would only wait another write
            // deadline on a peer that is not reading.
            eprintln!("agent-bridge server: IPC request read failed: {error:#}");
            return;
        }
    };
    let envelope = match decode_request(&frame) {
        Ok(envelope) => envelope,
        Err(error) => {
            let response = ResponseEnvelope::failure(
                "invalid-request",
                "invalid_request",
                format!("{error:#}"),
            );
            let _ = write_response(connection.get_mut(), &response);
            return;
        }
    };

    let request_id = envelope.id;
    let client_kind = envelope.client_kind;
    let client_session_id = envelope.client_session_id;
    let refresh_after_response = !matches!(envelope.request, Request::CloseClient);
    let stop_requested = matches!(envelope.request, Request::ServerStop);
    if let Some(client_session_id) = client_session_id.as_deref()
        && let Err(error) = state.host.touch_client(client_session_id)
    {
        let response =
            ResponseEnvelope::failure(request_id, "invalid_client_session", format!("{error:#}"));
        let _ = write_response(connection.get_mut(), &response);
        return;
    }
    let (response, stop_after_response) = match dispatch(
        &state,
        envelope.request,
        client_kind,
        client_session_id.as_deref(),
    ) {
        Ok((result, stop)) => (ResponseEnvelope::success(request_id, result), stop),
        Err(error) => (
            ResponseEnvelope::failure(request_id, "request_failed", format!("{error:#}")),
            // A ServerStop whose shutdown still failed must wake the accept
            // loop anyway so the process can exit; otherwise the loop would
            // wait for a client that never comes while the server holds the
            // session registry and its process ownership.
            stop_requested,
        ),
    };
    let wrote_response = write_response(connection.get_mut(), &response).is_ok();
    if wrote_response
        && response.ok
        && refresh_after_response
        && let Some(client_session_id) = client_session_id.as_deref()
    {
        let _ = state.host.touch_client(client_session_id);
    }
    if stop_after_response {
        wake_listener();
    }
}

fn dispatch(
    state: &RuntimeState,
    request: Request,
    client_kind: Option<crate::protocol::ClientKind>,
    client_session_id: Option<&str>,
) -> Result<(ResponseResult, bool)> {
    let result = match request {
        Request::ServerStatus => ResponseResult::ServerInfo(state.server_info()),
        Request::ServerStop => {
            state.stopping.store(true, Ordering::Release);
            state.host.shutdown_all()?;
            return Ok((ResponseResult::Accepted { accepted: true }, true));
        }
        Request::Heartbeat => {
            let client_session_id = client_session_id.context(
                "heartbeat requires CODEX_THREAD_ID, CODEX_SESSION_ID, or ANTIGRAVITY_CONVERSATION_ID in the client environment",
            )?;
            state.host.touch_client(client_session_id)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::CloseClient => {
            let client_session_id = client_session_id.context(
                "close_client requires CODEX_THREAD_ID, CODEX_SESSION_ID, or ANTIGRAVITY_CONVERSATION_ID in the client environment",
            )?;
            ResponseResult::CloseGroup(state.host.close_client(client_session_id)?)
        }
        Request::Create {
            worker,
            cwd,
            prompt,
            model,
            owner,
            always_approve,
            sandbox,
            dangerously_bypass,
        } => ResponseResult::Session(Box::new(state.host.create(
            worker,
            &cwd,
            prompt,
            model,
            owner,
            always_approve,
            sandbox,
            dangerously_bypass,
            client_kind,
            client_session_id.map(str::to_owned),
        )?)),
        Request::List => ResponseResult::Sessions {
            sessions: state.host.list()?,
        },
        Request::Show { session } => ResponseResult::Session(Box::new(state.host.show(&session)?)),
        Request::Read {
            session,
            cursor,
            limit,
            wait_ms,
        } => ResponseResult::Read(state.host.read(
            &session,
            cursor.unwrap_or(0),
            limit.unwrap_or(4096) as usize,
            wait_ms.unwrap_or(0),
        )?),
        Request::Send { session, input } => {
            ResponseResult::Session(Box::new(state.host.send(&session, input)?))
        }
        Request::Write {
            session,
            data_base64,
        } => {
            state
                .host
                .write_raw(&session, decode_write_data(&data_base64)?)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::Resize {
            session,
            cols,
            rows,
        } => {
            state.host.resize(&session, cols, rows)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::Wait {
            session,
            for_condition,
            timeout_ms,
        } => ResponseResult::Wait(state.host.wait(
            &session,
            for_condition,
            timeout_ms.unwrap_or(300_000),
        )?),
        Request::Close { session } => ResponseResult::Accepted {
            accepted: state.host.close(&session)?,
        },
        Request::HookEvent {
            worker,
            provider_session_id,
            event,
        } => ResponseResult::Accepted {
            accepted: state
                .host
                .apply_hook_event(worker, &provider_session_id, event)?,
        },
    };
    Ok((result, false))
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

fn wake_listener() {
    if let Ok(name) = runtime_name() {
        let _ = Stream::connect(name);
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
