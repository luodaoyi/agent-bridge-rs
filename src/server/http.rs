use std::{
    env, fmt,
    io::{self, BufRead, BufReader, ErrorKind, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    hooks::http_fallback::{HOOK_HTTP_ROUTE, HOOK_TOKEN_HEADER, MAX_HOOK_HTTP_BODY_BYTES},
    protocol::{validate_client_session_id, validate_owner},
    session::CloseError,
};

use super::{
    ConnectionSlot, RuntimeState, WEB_HTTP_MAX_HEADER_BYTES, WEB_HTTP_MAX_HEADER_COUNT,
    WEB_HTTP_MAX_HEADER_LINE_BYTES, WEB_HTTP_MAX_REQUEST_LINE_BYTES, WEB_HTTP_MAX_RESPONSE_BYTES,
    WEB_HTTP_POLL, WEB_HTTP_READ_TIMEOUT, WEB_HTTP_REJECT_DEADLINE, WEB_HTTP_REQUEST_DEADLINE,
    WEB_HTTP_WRITE_POLL_MAX, WEB_HTTP_WRITE_POLL_MIN, WEB_HTTP_WRITE_TIMEOUT,
    WEB_MAX_ACTIVE_CONNECTIONS, WEB_UI_CSS, WEB_UI_HTML, WEB_UI_JS, admit_connection,
    hook_http::handle_hook_http_post, ws::handle_events_websocket,
};

/// Default WebUI listener. Kept loopback-only because the WebUI has no user
/// authentication; `AGENT_BRIDGE_WEB_ADDR` may only select a loopback address.
const DEFAULT_WEB_ADDR: &str = "127.0.0.1:47654";

/// Why a `AGENT_BRIDGE_WEB_ADDR` value cannot be used as the WebUI listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WebAddrError {
    /// Not parseable as a literal `IpAddr:port`. Hostnames are never resolved.
    InvalidAddress(String),
    /// Parsed, but the IP is not a loopback address.
    NotLoopback(SocketAddr),
}

impl fmt::Display for WebAddrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebAddrError::InvalidAddress(value) => write!(
                formatter,
                "AGENT_BRIDGE_WEB_ADDR {value:?} is not a literal IP address with a port; \
                 the local-only WebUI never resolves hostnames, use a loopback address \
                 such as 127.0.0.1:47654 or [::1]:47654"
            ),
            WebAddrError::NotLoopback(address) => write!(
                formatter,
                "AGENT_BRIDGE_WEB_ADDR {address} is not a loopback address; the local-only \
                 WebUI may only bind to IPv4 127.0.0.0/8 or IPv6 ::1"
            ),
        }
    }
}

impl std::error::Error for WebAddrError {}

/// Parse a `AGENT_BRIDGE_WEB_ADDR` value into a bindable `SocketAddr` and
/// require it to be loopback (IPv4 127.0.0.0/8 or IPv6 ::1). Pure: never
/// resolves hostnames, never binds, never reads the environment.
pub(super) fn parse_web_addr(value: &str) -> Result<SocketAddr, WebAddrError> {
    match value.parse::<SocketAddr>() {
        Ok(address) if address.ip().is_loopback() => Ok(address),
        Ok(address) => Err(WebAddrError::NotLoopback(address)),
        Err(_) => Err(WebAddrError::InvalidAddress(value.to_owned())),
    }
}

/// Shared by `bind_web_ui` and tests; `value` is the raw `AGENT_BRIDGE_WEB_ADDR`.
pub(super) fn bind_web_ui_from(value: &str) -> Result<Option<TcpListener>> {
    // Parse and validate before binding so a wildcard, LAN, public, hostname,
    // or unresolvable value is a startup error and never reaches `bind`.
    let socket_addr = parse_web_addr(value)?;
    classify_web_ui_bind(value, TcpListener::bind(socket_addr))
}

pub(super) fn classify_web_ui_bind(
    value: &str,
    result: io::Result<TcpListener>,
) -> Result<Option<TcpListener>> {
    match result {
        Ok(listener) => Ok(Some(listener)),
        // A legal loopback address whose port is taken only disables the
        // WebUI; JSON CLI and PTY sessions keep running.
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            eprintln!("agent-bridge server: WebUI unavailable at {value}: {error}");
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("failed to bind WebUI at {value}")),
    }
}

pub(super) fn bind_web_ui() -> Result<Option<TcpListener>> {
    let address = env::var("AGENT_BRIDGE_WEB_ADDR").unwrap_or_else(|_| DEFAULT_WEB_ADDR.to_owned());
    bind_web_ui_from(&address)
}

pub(super) fn run_web_ui(listener: TcpListener, state: Arc<RuntimeState>) {
    for connection in listener.incoming() {
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        match connection {
            Ok(mut stream) => {
                if !admit_connection(&state.web_connections, WEB_MAX_ACTIVE_CONNECTIONS) {
                    // At the Web connection cap: answer with a bounded 503
                    // under a short total deadline and close without spawning
                    // another handler thread, so a peer that never drains
                    // cannot pin the accept loop.
                    let _ = write_http_bytes_with_deadline(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        b"too many active WebUI connections; retry shortly",
                        WEB_HTTP_REJECT_DEADLINE,
                    );
                    continue;
                }
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    // Keep a dedicated Arc for the slot so the counter stays alive for
                    // the whole handler while `state` moves into the handler.
                    let slot_state = Arc::clone(&state);
                    let _slot = ConnectionSlot {
                        slots: &slot_state.web_connections,
                    };
                    handle_web_connection(stream, state);
                });
            }
            Err(error) => eprintln!("agent-bridge server: WebUI accept failed: {error}"),
        }
    }
}

pub(super) fn handle_web_connection(mut stream: TcpStream, state: Arc<RuntimeState>) {
    let _ = stream.set_read_timeout(Some(WEB_HTTP_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WEB_HTTP_WRITE_TIMEOUT));
    let request = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let _ = write_http(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                &error,
            );
            return;
        }
    };
    if request.method == "GET" && request.path == "/api/events" {
        handle_events_websocket(stream, state, request);
        return;
    }
    if request.method == "GET"
        && let Some(asset) = static_web_asset(&request.path)
    {
        let _ = write_http_bytes(&mut stream, "200 OK", asset.content_type, asset.body);
        return;
    }
    let method = request.method.as_str();
    let path = request.path.as_str();
    let bridge_header = request.bridge_header;
    match (method, path) {
        ("POST", HOOK_HTTP_ROUTE) => {
            handle_hook_http_post(&mut stream, &state, request);
        }
        ("GET", "/api/sessions") => match state.host.list_web().and_then(|sessions| {
            serde_json::to_string(&sessions).context("failed to encode WebUI sessions")
        }) {
            Ok(body) => {
                let _ = write_http(&mut stream, "200 OK", "application/json", &body);
            }
            Err(error) => {
                let _ = write_http(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    &format!("{error:#}"),
                );
            }
        },
        ("GET", "/api/version") => {
            let body = serde_json::to_string(&state.version_checker.status())
                .unwrap_or_else(|_| {
                    r#"{"current":"unknown","update_available":false,"release_url":"https://github.com/luodaoyi/agent-bridge-rs/releases/latest"}"#.to_owned()
                });
            let _ = write_http(&mut stream, "200 OK", "application/json", &body);
        }
        ("POST", path) if path.starts_with("/api/clients/") => {
            let Some(encoded_client) = close_path_segment(path, "/api/clients/") else {
                let _ = write_http(
                    &mut stream,
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    "not found",
                );
                return;
            };
            if !bridge_header {
                let _ = write_http(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain; charset=utf-8",
                    "missing WebUI request header",
                );
                return;
            }
            let client_session_id = match percent_decode_path_segment(encoded_client) {
                Ok(client_session_id) => client_session_id,
                Err(error) => {
                    let _ = write_http(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        &error,
                    );
                    return;
                }
            };
            if let Err(error) = validate_client_session_id(&client_session_id) {
                let _ = write_http(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    &format!("{error:#}"),
                );
                return;
            }
            match state.host.close_client(&client_session_id) {
                Ok(result) => {
                    let body = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"matched":0,"closed":0,"timed_out":[],"failures":[]}"#.to_owned()
                    });
                    let _ = write_http(&mut stream, "200 OK", "application/json", &body);
                }
                Err(error) => {
                    let _ = write_http(
                        &mut stream,
                        "500 Internal Server Error",
                        "text/plain; charset=utf-8",
                        &format!("{error:#}"),
                    );
                }
            }
        }
        ("POST", path) if path.starts_with("/api/owners/") => {
            let Some(encoded_owner) = close_path_segment(path, "/api/owners/") else {
                let _ = write_http(
                    &mut stream,
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    "not found",
                );
                return;
            };
            if !bridge_header {
                let _ = write_http(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain; charset=utf-8",
                    "missing WebUI request header",
                );
                return;
            }
            let owner = match percent_decode_path_segment(encoded_owner) {
                Ok(owner) => owner,
                Err(error) => {
                    let _ = write_http(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        &error,
                    );
                    return;
                }
            };
            if let Err(error) = validate_owner(&owner) {
                let _ = write_http(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    &format!("{error:#}"),
                );
                return;
            }
            match state.host.close_owner(&owner) {
                Ok(result) => {
                    let body = serde_json::json!({
                        "matched": result.matched,
                        "closed": result.closed,
                        "timed_out": result.timed_out,
                        "failures": result.failures,
                    })
                    .to_string();
                    let _ = write_http(&mut stream, "200 OK", "application/json", &body);
                }
                Err(error) => {
                    let _ = write_http(
                        &mut stream,
                        "500 Internal Server Error",
                        "text/plain; charset=utf-8",
                        &format!("{error:#}"),
                    );
                }
            }
        }
        ("POST", path) if path.starts_with("/api/sessions/") => {
            let Some(handle) = close_path_segment(path, "/api/sessions/") else {
                let _ = write_http(
                    &mut stream,
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    "not found",
                );
                return;
            };
            if !bridge_header {
                let _ = write_http(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain; charset=utf-8",
                    "missing WebUI request header",
                );
                return;
            }
            match state.host.close(handle) {
                Ok(closed) => {
                    let body = format!(r#"{{"accepted":{closed}}}"#);
                    let _ = write_http(&mut stream, "200 OK", "application/json", &body);
                }
                Err(error) => {
                    let _ = write_http(
                        &mut stream,
                        close_error_http_status(&error),
                        "text/plain; charset=utf-8",
                        &error.to_string(),
                    );
                }
            }
        }
        _ => {
            let _ = write_http(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found",
            );
        }
    }
}

fn close_error_http_status(error: &CloseError) -> &'static str {
    match error {
        CloseError::NotFound(_) => "404 Not Found",
        CloseError::Timeout => "504 Gateway Timeout",
        CloseError::Failed(_) => "500 Internal Server Error",
    }
}

/// Same-origin browser Origin on a loopback Host only.
pub(super) fn web_origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(host) = host.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(origin_authority) = http_origin_authority(origin) else {
        return false;
    };
    if !authority_is_loopback(&origin_authority) || !authority_is_loopback(host) {
        return false;
    }
    authority_eq(&origin_authority, host)
}

fn http_origin_authority(origin: &str) -> Option<String> {
    let rest = origin.strip_prefix("http://")?;
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    Some(rest.to_owned())
}

fn authority_is_loopback(authority: &str) -> bool {
    let host = authority_host(authority);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn authority_host(authority: &str) -> &str {
    if authority.starts_with('[') {
        if let Some(end) = authority.find(']') {
            return &authority[..=end];
        }
        return authority;
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && port.chars().all(|ch| ch.is_ascii_digit())
    {
        return host;
    }
    authority
}

fn authority_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

pub(super) fn close_path_segment<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let segment = path.strip_prefix(prefix)?.strip_suffix("/close")?;
    if segment.contains('/') || segment.contains('?') {
        return None;
    }
    Some(segment)
}

pub(super) fn percent_decode_path_segment(value: &str) -> std::result::Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("owner contains an incomplete percent escape".to_owned());
            }
            let high = hex_value(bytes[index + 1])
                .ok_or_else(|| "owner contains an invalid percent escape".to_owned())?;
            let low = hex_value(bytes[index + 2])
                .ok_or_else(|| "owner contains an invalid percent escape".to_owned())?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "owner is not valid UTF-8".to_owned())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct ParsedHttpRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) bridge_header: bool,
    pub(super) hook_token: Option<String>,
    pub(super) host: Option<String>,
    pub(super) origin: Option<String>,
    pub(super) upgrade_websocket: bool,
    pub(super) connection_upgrade: bool,
    pub(super) sec_websocket_key: Option<String>,
    pub(super) sec_websocket_version: Option<String>,
    pub(super) body: Vec<u8>,
    pub(super) body_error: Option<String>,
}

fn read_http_request(stream: &mut TcpStream) -> std::result::Result<ParsedHttpRequest, String> {
    read_http_request_with_deadline(stream, Instant::now() + WEB_HTTP_REQUEST_DEADLINE)
}

/// Parse one HTTP request, bounded by the documented request line/header caps
/// and a wall-clock deadline. `deadline` is injectable for tests.
pub(super) fn read_http_request_with_deadline(
    stream: &mut TcpStream,
    deadline: Instant,
) -> std::result::Result<ParsedHttpRequest, String> {
    let mut reader = BufReader::new(stream);
    let request_line = read_http_line(&mut reader, WEB_HTTP_MAX_REQUEST_LINE_BYTES, deadline)?;
    let request_line = std::str::from_utf8(&request_line)
        .map_err(|_| "request line is not valid UTF-8".to_owned())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing HTTP method")?.to_owned();
    let path = parts.next().ok_or("missing HTTP path")?.to_owned();
    let mut bridge_header = false;
    let mut hook_token = None;
    let mut host = None;
    let mut origin = None;
    let mut upgrade_websocket = false;
    let mut connection_upgrade = false;
    let mut sec_websocket_key = None;
    let mut sec_websocket_version = None;
    let mut content_length: Option<usize> = None;
    let mut body_error: Option<String> = None;
    let mut header_count = 0usize;
    let mut header_bytes = 0usize;
    loop {
        let line = read_http_line(&mut reader, WEB_HTTP_MAX_HEADER_LINE_BYTES, deadline)?;
        if line.is_empty() {
            break;
        }
        header_count += 1;
        header_bytes += line.len();
        if header_count > WEB_HTTP_MAX_HEADER_COUNT {
            return Err(format!(
                "too many HTTP headers; the limit is {WEB_HTTP_MAX_HEADER_COUNT}"
            ));
        }
        if header_bytes > WEB_HTTP_MAX_HEADER_BYTES {
            return Err(format!(
                "HTTP headers exceed the total size limit of {WEB_HTTP_MAX_HEADER_BYTES} bytes"
            ));
        }
        let header =
            std::str::from_utf8(&line).map_err(|_| "header is not valid UTF-8".to_owned())?;
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("X-Agent-Bridge-WebUI") && value == "1" {
            bridge_header = true;
        } else if name.eq_ignore_ascii_case(HOOK_TOKEN_HEADER) {
            hook_token = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("Content-Length") {
            match value.parse::<usize>() {
                Ok(length) if length <= MAX_HOOK_HTTP_BODY_BYTES => content_length = Some(length),
                Ok(_) => {
                    body_error = Some(format!(
                        "HTTP body exceeds the {MAX_HOOK_HTTP_BODY_BYTES}-byte size limit"
                    ));
                }
                Err(_) => body_error = Some("invalid Content-Length header".to_owned()),
            }
        } else if name.eq_ignore_ascii_case("Host") {
            host = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("Origin") {
            origin = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("Upgrade") && value.eq_ignore_ascii_case("websocket") {
            upgrade_websocket = true;
        } else if name.eq_ignore_ascii_case("Connection") {
            connection_upgrade = value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"));
        } else if name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
            sec_websocket_key = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("Sec-WebSocket-Version") {
            sec_websocket_version = Some(value.to_owned());
        }
    }
    let body = if body_error.is_none()
        && let Some(length) = content_length.filter(|&length| length > 0)
    {
        read_http_body(&mut reader, length, deadline)?
    } else {
        Vec::new()
    };
    Ok(ParsedHttpRequest {
        method,
        path,
        bridge_header,
        hook_token,
        host,
        origin,
        upgrade_websocket,
        connection_upgrade,
        sec_websocket_key,
        sec_websocket_version,
        body,
        body_error,
    })
}

fn read_http_body(
    reader: &mut impl BufRead,
    length: usize,
    deadline: Instant,
) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::with_capacity(length);
    while body.len() < length {
        if Instant::now() >= deadline {
            return Err("HTTP request body read timed out".to_owned());
        }
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(WEB_HTTP_POLL);
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if buffer.is_empty() {
            return Err("unexpected EOF while reading HTTP request body".to_owned());
        }
        let needed = length - body.len();
        let take = buffer.len().min(needed);
        body.extend_from_slice(&buffer[..take]);
        reader.consume(take);
    }
    Ok(body)
}

/// Read one CRLF/LF-terminated line with a hard byte cap and a wall-clock
/// deadline, so a hostile or broken peer can neither make the server allocate
/// an unbounded line nor hold a handler thread past the request budget.
///
/// An empty line (blank separator or EOF) yields an empty `Vec`. A line longer
/// than `cap` or a request that outlives `deadline` fails the parse; idle reads
/// are retried with a short sleep (mirroring the IPC frame poll) so a trickling
/// client cannot stall the deadline.
pub(super) fn read_http_line(
    reader: &mut impl BufRead,
    cap: usize,
    deadline: Instant,
) -> std::result::Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err("HTTP request read timed out".to_owned());
        }
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(WEB_HTTP_POLL);
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if buffer.is_empty() {
            return Ok(line);
        }
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&buffer[..index]);
            reader.consume(index + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > cap {
                return Err(format!("HTTP line exceeds the {cap}-byte size limit"));
            }
            return Ok(line);
        }
        let length = buffer.len();
        line.extend_from_slice(buffer);
        reader.consume(length);
        if line.len() > cap {
            return Err(format!("HTTP line exceeds the {cap}-byte size limit"));
        }
    }
}

pub(super) fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write_http_bytes(stream, status, content_type, body.as_bytes())
}

pub(super) fn write_http_bytes(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_http_bytes_with_deadline(stream, status, content_type, body, WEB_HTTP_WRITE_TIMEOUT)
}

/// Like [`write_http_bytes`], but under an explicit total deadline. Used by
/// the connection-cap rejection path, which writes directly from the accept
/// thread and must not be pinned by a peer that never drains.
fn write_http_bytes_with_deadline(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    deadline: Duration,
) -> std::io::Result<()> {
    if body.len() > WEB_HTTP_MAX_RESPONSE_BYTES {
        // Keep the wire response bounded even if a route produced an oversized
        // body; the replacement body is tiny and cannot recurse.
        return write_http_bytes_with_deadline(
            stream,
            "500 Internal Server Error",
            "text/plain; charset=utf-8",
            b"server response exceeds the size limit",
            deadline,
        );
    }
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    let mut response = Vec::with_capacity(headers.len() + body.len());
    response.extend_from_slice(headers.as_bytes());
    response.extend_from_slice(body);
    write_tcp_all_with_deadline(stream, &response, deadline)
}

/// Minimal write surface for [`write_tcp_all_with_deadline`]: a blocking
/// `write` plus a per-syscall write timeout. The deadline loop is generic
/// over this trait so tests can drive it with a fake writer that reports
/// `WouldBlock` forever, instead of depending on kernel socket buffers,
/// which std::net does not expose and whose sizes differ per platform.
pub(super) trait DeadlineWrite: Write {
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

impl DeadlineWrite for TcpStream {
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

/// Write one bounded HTTP response under a total wall-clock deadline. A
/// socket write timeout alone is per syscall, so a peer that accepts a few
/// bytes repeatedly could otherwise keep resetting `write_all` forever.
/// Blocked writes (full receive buffer) are retried with a bounded 1–50 ms
/// backoff instead of a busy loop, and the per-syscall timeout is never set
/// below 1 ms so a nearly-expired deadline cannot degrade into spinning.
pub(super) fn write_tcp_all_with_deadline(
    stream: &mut impl DeadlineWrite,
    mut data: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut poll_delay = WEB_HTTP_WRITE_POLL_MIN;
    while !data.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "HTTP response write exceeded its total I/O deadline",
            ));
        }
        stream.set_write_timeout(Some(
            deadline
                .saturating_duration_since(now)
                .max(WEB_HTTP_WRITE_POLL_MIN),
        ))?;
        match stream.write(data) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "peer closed while receiving the HTTP response",
                ));
            }
            Ok(written) => {
                data = &data[written..];
                poll_delay = WEB_HTTP_WRITE_POLL_MIN;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "HTTP response write exceeded its total I/O deadline",
                    ));
                }
                thread::sleep(poll_delay.min(remaining));
                poll_delay = (poll_delay * 2).min(WEB_HTTP_WRITE_POLL_MAX);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) struct StaticWebAsset {
    pub(super) content_type: &'static str,
    pub(super) body: &'static [u8],
}

pub(super) fn static_web_asset(path: &str) -> Option<StaticWebAsset> {
    match path {
        "/" => Some(StaticWebAsset {
            content_type: "text/html; charset=utf-8",
            body: WEB_UI_HTML,
        }),
        "/assets/app.js" => Some(StaticWebAsset {
            content_type: "text/javascript; charset=utf-8",
            body: WEB_UI_JS,
        }),
        "/assets/app.css" => Some(StaticWebAsset {
            content_type: "text/css; charset=utf-8",
            body: WEB_UI_CSS,
        }),
        _ => None,
    }
}
