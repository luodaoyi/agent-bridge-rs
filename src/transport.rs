#[cfg(any(windows, test))]
use std::io::Read;
use std::{
    env,
    io::{BufRead, BufReader, ErrorKind, Write},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use interprocess::local_socket::{Name, Stream, prelude::*};

mod platform;
#[cfg(test)]
mod tests;

pub(crate) use platform::runtime_name;
use platform::start_detached_server;
#[cfg(unix)]
pub(crate) use platform::{
    RuntimeStartupLock, acquire_runtime_startup_lock, bind_listener_at, legacy_runtime_name,
    remove_stale_runtime_socket, runtime_identity, runtime_socket_path,
};
// Keep the existing crate-visible platform API available at this entry point.
#[cfg(unix)]
#[allow(unused_imports)]
pub(crate) use platform::{
    RUNTIME_DIR_MODE, RUNTIME_SOCKET_MODE, ensure_runtime_dir, runtime_dir, verify_runtime_socket,
};
#[cfg(all(unix, test))]
pub(crate) use tests::ShortTempDir;

use crate::protocol::{
    ClientKind, MAX_FRAME_BYTES, Request, RequestEnvelope, ResponseEnvelope, decode_response,
    encode_frame, validate_client_session_id,
};

const START_RETRIES: usize = 50;
const START_RETRY_DELAY: Duration = Duration::from_millis(100);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Server-side deadline for receiving a complete request frame from an IPC
/// client. A connected-but-silent or trickling client never pins a handler
/// thread beyond this.
pub(crate) const IPC_FRAME_READ_DEADLINE: Duration = Duration::from_secs(30);
/// Deadline for any single frame write (client request or server response).
/// Frames are at most 1 MiB and written in one burst, so a peer that stops
/// draining surfaces an error within this window.
pub(crate) const IPC_WRITE_DEADLINE: Duration = Duration::from_secs(30);
/// Short total deadline for a connection-cap rejection frame written on the
/// accept thread: a peer that never drains must not pin acceptance of further
/// clients for the full IPC write deadline.
pub(crate) const IPC_REJECT_DEADLINE: Duration = Duration::from_secs(2);
/// Poll backoff bounds while an IPC peer is idle, so a waiting thread sleeps
/// instead of spinning yet still notices data promptly.
const IPC_POLL_MIN: Duration = Duration::from_millis(1);
const IPC_POLL_MAX: Duration = Duration::from_millis(50);
/// Extra headroom the client grants the server beyond the request's own
/// documented timeout (`read` wait and `wait` timeout are bounded server-side
/// in session.rs) so legitimate long operations are never cut short.
const IPC_RESPONSE_SLACK: Duration = Duration::from_secs(60);

pub(crate) fn call(request: Request, auto_start: bool) -> Result<ResponseEnvelope> {
    let identity = current_client_identity()?;
    let (client_kind, client_session_id) = match identity {
        Some((kind, id)) => (Some(kind), Some(id)),
        None => (None, None),
    };
    call_with_client_session(request, auto_start, client_kind, client_session_id)
}

pub(crate) fn call_anonymous(request: Request, auto_start: bool) -> Result<ResponseEnvelope> {
    call_with_client_session(request, auto_start, None, None)
}

fn call_with_client_session(
    request: Request,
    auto_start: bool,
    client_kind: Option<ClientKind>,
    client_session_id: Option<String>,
) -> Result<ResponseEnvelope> {
    let envelope = RequestEnvelope {
        id: next_request_id(),
        client_kind,
        client_session_id,
        request,
    };
    let stream = match connect() {
        Ok(stream) => stream,
        Err(first_error) if auto_start => {
            start_detached_server().context("failed to launch the agent-bridge runtime server")?;
            let mut last_error = first_error;
            for _ in 0..START_RETRIES {
                thread::sleep(START_RETRY_DELAY);
                match connect() {
                    Ok(stream) => return call_over_stream(stream, &envelope),
                    Err(error) => last_error = error,
                }
            }
            return Err(last_error)
                .context("runtime server did not become ready within five seconds");
        }
        Err(error) => return Err(error),
    };
    call_over_stream(stream, &envelope)
}

fn current_client_identity() -> Result<Option<(ClientKind, String)>> {
    client_identity_from(
        env::var("AGENT_BRIDGE_CLIENT_KIND").ok(),
        env::var("AGENT_BRIDGE_CLIENT_SESSION_ID").ok(),
        env::var("CODEX_THREAD_ID").ok(),
        env::var("CODEX_SESSION_ID").ok(),
        env::var("ANTIGRAVITY_CONVERSATION_ID").ok(),
        env::var("ANTIGRAVITY_TRAJECTORY_ID").ok(),
    )
}

fn client_identity_from(
    explicit_kind: Option<String>,
    explicit_session_id: Option<String>,
    codex_thread_id: Option<String>,
    codex_session_id: Option<String>,
    agy_conversation_id: Option<String>,
    agy_trajectory_id: Option<String>,
) -> Result<Option<(ClientKind, String)>> {
    let non_empty = |value: Option<String>| value.filter(|v| !v.trim().is_empty());

    let parsed_explicit_kind = explicit_kind
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<ClientKind>())
        .transpose()?;

    let resolved = non_empty(explicit_session_id)
        .map(|id| (parsed_explicit_kind.unwrap_or(ClientKind::Unknown), id))
        .or_else(|| {
            non_empty(codex_thread_id)
                .or_else(|| non_empty(codex_session_id))
                .map(|id| (parsed_explicit_kind.unwrap_or(ClientKind::Codex), id))
        })
        .or_else(|| {
            non_empty(agy_conversation_id)
                .or_else(|| non_empty(agy_trajectory_id))
                .map(|id| (parsed_explicit_kind.unwrap_or(ClientKind::Agy), id))
        });

    let Some((kind, raw_id)) = resolved else {
        return Ok(None);
    };
    validate_client_session_id(&raw_id)?;
    let scoped = if raw_id.starts_with("codex:")
        || raw_id.starts_with("agy:")
        || raw_id.starts_with("unknown:")
    {
        raw_id
    } else {
        format!("{}:{}", kind.as_str(), raw_id)
    };
    validate_client_session_id(&scoped)?;
    Ok(Some((kind, scoped)))
}

/// Connect to the Runtime IPC endpoint. On Unix the client tries the current
/// filesystem endpoint first and, when that fails, probes the legacy
/// GenericNamespaced endpoint so a Runtime started by an older binary is still
/// found and used instead of starting a second one. Only when neither endpoint
/// answers does the caller fall back to auto-starting a new Runtime. A
/// candidate whose name cannot even be constructed (e.g. a path that would
/// overflow the socket address) is skipped, never fatal: the legacy endpoint
/// must still be probed. If every candidate fails to construct, that error is
/// preserved for diagnosis instead of being replaced by a generic connect
/// failure.
fn connect() -> Result<Stream> {
    #[cfg(unix)]
    let candidates = [runtime_name(), legacy_runtime_name()];
    #[cfg(not(unix))]
    let candidates = [runtime_name()];
    let mut names = Vec::with_capacity(candidates.len());
    let mut construction_error = None;
    for candidate in candidates {
        match candidate {
            Ok(name) => names.push(name),
            Err(error) => {
                if construction_error.is_none() {
                    construction_error = Some(error);
                }
            }
        }
    }
    connect_first(&names).map_err(|error| match construction_error {
        Some(name_error) => error.context(format!(
            "a runtime endpoint name could not be constructed: {name_error:#}"
        )),
        None => error,
    })
}

/// Try each IPC endpoint in order and return the first live connection.
/// `candidates` are injected so tests can prove the fallback order against
/// throwaway names without touching the real runtime endpoints. An empty
/// candidate list is a diagnosable error, never a panic.
fn connect_first(candidates: &[Name<'_>]) -> Result<Stream> {
    let mut last_error = None;
    for name in candidates {
        match Stream::connect(name.clone()) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error).context("runtime server is not running"),
        None => bail!("no runtime IPC endpoint candidates are available"),
    }
}

/// Windows named pipes in PIPE_NOWAIT mode report "no data yet" as a
/// successful zero-byte read (`ERROR_NO_DATA` → `Ok(0)`), not `WouldBlock`.
/// `BufReader` treats `Ok(0)` as sticky EOF, so a reader that polls before
/// the peer has written would permanently see "protocol peer closed" even
/// after the bytes arrive. Remap empty reads to `WouldBlock` *before* they
/// reach `BufReader`. Real disconnects still surface as `ERROR_BROKEN_PIPE`
/// / `ERROR_PIPE_NOT_CONNECTED` errors. Used on the client response-read
/// path and the server request-read path. Unix is not wrapped: Unix `Ok(0)`
/// is real EOF. Matches `write_frame_all`, which treats Windows `Ok(0)` as
/// backpressure and shrinks an all-or-nothing nowait write that could not
/// fit in the remaining pipe buffer.
#[cfg(any(windows, test))]
pub(crate) struct RemapNowaitEmptyRead<R>(pub(crate) R);

#[cfg(any(windows, test))]
impl<R: Read> Read for RemapNowaitEmptyRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.read(buf) {
            Ok(0) => Err(std::io::Error::from(ErrorKind::WouldBlock)),
            other => other,
        }
    }
}

#[cfg(any(windows, test))]
impl<W: Write> Write for RemapNowaitEmptyRead<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn call_over_stream(stream: Stream, envelope: &RequestEnvelope) -> Result<ResponseEnvelope> {
    // Non-blocking I/O lets both sides enforce I/O deadlines on every platform
    // (Windows named pipes do not support native socket timeouts).
    stream
        .set_nonblocking(true)
        .context("failed to enable non-blocking runtime I/O")?;
    let response_deadline = Instant::now() + response_deadline_for(&envelope.request);
    #[cfg(windows)]
    let mut connection = BufReader::new(RemapNowaitEmptyRead(stream));
    #[cfg(not(windows))]
    let mut connection = BufReader::new(stream);
    write_frame_all(
        connection.get_mut(),
        &encode_frame(envelope)?,
        IPC_WRITE_DEADLINE,
    )
    .context("failed to write runtime request")?;
    let frame = read_frame(&mut connection, Some(response_deadline))
        .context("failed to read runtime response")?;
    let response = decode_response(&frame)?;
    if response.id != envelope.id {
        bail!(
            "runtime response id mismatch: expected {}, received {}",
            envelope.id,
            response.id
        );
    }
    Ok(response)
}

/// Response read deadline mirroring the server's per-operation timeouts plus
/// slack. `Session::read` clamps its wait to 300 s and `Session::wait` clamps
/// its timeout to 7_200_000 ms (session.rs); a request that uses the full
/// documented timeout must not be cut short by an unrelated default deadline.
fn response_deadline_for(request: &Request) -> Duration {
    let base_ms = match request {
        Request::Read { wait_ms, .. } => wait_ms.unwrap_or(0).min(300_000),
        Request::Wait { timeout_ms, .. } => timeout_ms.unwrap_or(300_000).min(7_200_000),
        _ => 0,
    };
    Duration::from_millis(base_ms) + IPC_RESPONSE_SLACK
}
/// Read one NDJSON frame, bounded by `MAX_FRAME_BYTES` and an optional read
/// deadline.
///
/// With `deadline` set, the peer is expected to keep the stream non-blocking:
/// idle reads poll with a short backoff and the call fails once the deadline
/// passes, so a silent or trickling peer can never pin the calling thread
/// indefinitely. With `deadline = None` the read blocks like a plain
/// `fill_buf` loop and preserves the historical behavior.
pub(crate) fn read_frame(reader: &mut impl BufRead, deadline: Option<Instant>) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(4096);
    let mut poll_delay = IPC_POLL_MIN;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            bail!(
                "protocol frame read timed out; the peer did not complete the frame within the I/O deadline"
            );
        }
        let buffer = match reader.fill_buf() {
            Ok(buffer) => {
                poll_delay = IPC_POLL_MIN;
                buffer
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if deadline.is_none() {
                    return Err(error).context("failed to buffer protocol data");
                }
                thread::sleep(poll_delay);
                poll_delay = (poll_delay * 2).min(IPC_POLL_MAX);
                continue;
            }
            Err(error) => return Err(error).context("failed to buffer protocol data"),
        };
        if buffer.is_empty() {
            if frame.is_empty() {
                bail!("protocol peer closed before sending a frame");
            }
            return Ok(frame);
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if frame.len() + length > MAX_FRAME_BYTES {
            bail!("protocol frame exceeds the 1 MiB limit");
        }
        frame.extend_from_slice(&buffer[..length]);
        reader.consume(length);
        if frame.last() == Some(&b'\n') {
            return Ok(frame);
        }
    }
}

/// Write `data` to the stream within `deadline`, polling with backoff while
/// the peer's receive buffer is full. Non-blocking peers that stop draining
/// produce a bounded write error instead of blocking the caller forever.
///
/// Windows named pipes in nonblocking (PIPE_NOWAIT) mode report a full buffer
/// as `Ok(0)`: a successful zero-byte completion, not a peer close. WriteFile
/// is also all-or-nothing: a slice larger than the remaining pipe buffer
/// completes with `Ok(0)` instead of a partial write. Retrying that same
/// oversized slice never makes progress — even when the peer is reading —
/// so the next attempt is shrunk until a slice fits. A 1-byte `Ok(0)` is a
/// real stall and uses the same bounded backoff. On every other platform
/// `Ok(0)` still means the peer closed the stream.
///
/// There is deliberately no trailing `flush()`: these frames are written
/// directly to the underlying OS handle (never through a userspace
/// `BufWriter`), so there is no buffered data to flush, and a flush on a
/// Windows named pipe (`FlushFileBuffers`) can block until the peer reads —
/// exactly the unbounded wait this function exists to avoid.
fn write_frame_all(stream: &mut impl Write, data: &[u8], deadline: Duration) -> Result<()> {
    write_frame_all_with(stream, data, deadline, cfg!(windows))
}

fn write_frame_all_with(
    stream: &mut impl Write,
    mut data: &[u8],
    deadline: Duration,
    windows_nowait_zero: bool,
) -> Result<()> {
    let deadline = Instant::now() + deadline;
    let mut poll_delay = IPC_POLL_MIN;
    let mut write_cap = data.len().max(1);
    while !data.is_empty() {
        if Instant::now() >= deadline {
            bail!(
                "protocol frame write timed out; the peer did not drain the data within the I/O deadline"
            );
        }
        let pending = &data[..data.len().min(write_cap)];
        let stalled = match stream.write(pending) {
            Ok(0) if windows_nowait_zero => windows_nowait_write_zero_is_stall(&mut write_cap),
            Ok(0) => bail!("protocol peer closed while receiving the frame"),
            Ok(written) => {
                data = &data[written..];
                poll_delay = IPC_POLL_MIN;
                false
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                true
            }
            Err(error) => return Err(error.into()),
        };
        if stalled {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "protocol frame write timed out; the peer did not drain the data within the I/O deadline"
                );
            }
            thread::sleep(poll_delay.min(remaining));
            poll_delay = (poll_delay * 2).min(IPC_POLL_MAX);
        }
    }
    Ok(())
}

/// After a Windows PIPE_NOWAIT `write` returned `Ok(0)`, shrink the next
/// attempt if the slice may have been larger than the remaining pipe buffer.
/// Returns `true` when even a 1-byte write made no progress (real stall).
fn windows_nowait_write_zero_is_stall(write_cap: &mut usize) -> bool {
    if *write_cap > 1 {
        *write_cap /= 2;
        false
    } else {
        true
    }
}

pub(crate) fn write_response(stream: &mut impl Write, response: &ResponseEnvelope) -> Result<()> {
    write_response_with_deadline(stream, response, IPC_WRITE_DEADLINE)
}

/// Like [`write_response`], but under an explicit total deadline — used for
/// connection-cap rejections written directly from the accept thread, where
/// a slow peer must not stall acceptance of further clients.
pub(crate) fn write_response_with_deadline(
    stream: &mut impl Write,
    response: &ResponseEnvelope,
    deadline: Duration,
) -> Result<()> {
    write_frame_all(stream, &encode_frame(response)?, deadline)
        .context("failed to write runtime response")
}

fn next_request_id() -> String {
    let sequence = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "req-{:x}-{:x}-{sequence:x}",
        std::process::id(),
        now_millis()
    )
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
