use std::{
    io::{BufRead, BufReader, ErrorKind},
    net::TcpStream,
    thread,
    time::Instant,
};

use crate::hooks::http_fallback::{HOOK_TOKEN_HEADER, MAX_HOOK_HTTP_BODY_BYTES};

use super::super::{
    WEB_HTTP_MAX_HEADER_BYTES, WEB_HTTP_MAX_HEADER_COUNT, WEB_HTTP_MAX_HEADER_LINE_BYTES,
    WEB_HTTP_MAX_REQUEST_LINE_BYTES, WEB_HTTP_POLL, WEB_HTTP_REQUEST_DEADLINE,
};

#[derive(Clone, Debug, Default)]
pub(in crate::server) struct ParsedHttpRequest {
    pub(in crate::server) method: String,
    pub(in crate::server) path: String,
    pub(in crate::server) bridge_header: bool,
    pub(in crate::server) hook_token: Option<String>,
    pub(in crate::server) host: Option<String>,
    pub(in crate::server) origin: Option<String>,
    pub(in crate::server) upgrade_websocket: bool,
    pub(in crate::server) connection_upgrade: bool,
    pub(in crate::server) sec_websocket_key: Option<String>,
    pub(in crate::server) sec_websocket_version: Option<String>,
    pub(in crate::server) body: Vec<u8>,
    pub(in crate::server) body_error: Option<String>,
}

pub(super) fn read_http_request(
    stream: &mut TcpStream,
) -> std::result::Result<ParsedHttpRequest, String> {
    read_http_request_with_deadline(stream, Instant::now() + WEB_HTTP_REQUEST_DEADLINE)
}

/// Parse one HTTP request, bounded by the documented request line/header caps
/// and a wall-clock deadline. `deadline` is injectable for tests.
pub(in crate::server) fn read_http_request_with_deadline(
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
pub(in crate::server) fn read_http_line(
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
