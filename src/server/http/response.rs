use std::{
    io::{self, ErrorKind, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use super::super::{
    WEB_HTTP_MAX_RESPONSE_BYTES, WEB_HTTP_WRITE_POLL_MAX, WEB_HTTP_WRITE_POLL_MIN,
    WEB_HTTP_WRITE_TIMEOUT,
};

pub(in crate::server) fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write_http_bytes(stream, status, content_type, body.as_bytes())
}

pub(in crate::server) fn write_http_bytes(
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
pub(super) fn write_http_bytes_with_deadline(
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
pub(in crate::server) trait DeadlineWrite: Write {
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
pub(in crate::server) fn write_tcp_all_with_deadline(
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
