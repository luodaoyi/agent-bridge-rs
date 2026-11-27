use std::{io::Write, net::TcpStream, sync::Arc, time::Duration};

use anyhow::Result;
use tungstenite::{
    WebSocket,
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
};

use super::{
    super::{
        RuntimeState, WEB_EVENTS_MAX_MESSAGE_BYTES, WEB_HTTP_WRITE_TIMEOUT,
        http::{ParsedHttpRequest, web_origin_allowed, write_http},
    },
    connection::run_events_websocket,
};

pub(in crate::server) fn handle_events_websocket(
    mut stream: TcpStream,
    state: Arc<RuntimeState>,
    request: ParsedHttpRequest,
) {
    if let Err(error) = validate_events_websocket_request(&request) {
        let status = if error.starts_with("origin") {
            "403 Forbidden"
        } else {
            "400 Bad Request"
        };
        let _ = write_http(&mut stream, status, "text/plain; charset=utf-8", &error);
        return;
    }
    let Some(key) = request.sec_websocket_key.as_deref() else {
        let _ = write_http(
            &mut stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            "missing Sec-WebSocket-Key",
        );
        return;
    };
    let accept = derive_accept_key(key.as_bytes());
    if write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .is_err()
    {
        return;
    }
    // Host Condvar is the primary sleep; keep socket reads short so a quiet
    // client socket cannot delay event pushes after a revision wake-up.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1)));
    let _ = stream.set_write_timeout(Some(WEB_HTTP_WRITE_TIMEOUT));

    let mut websocket = WebSocket::from_raw_socket(stream, Role::Server, Some(web_socket_config()));
    run_events_websocket(&mut websocket, &state);
}

/// Bounded WebSocket configuration: every frame, message, read and write
/// buffer has an explicit cap so a hostile or broken peer can never grow
/// memory without bound or pin the connection thread.
pub(in crate::server) fn web_socket_config() -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(WEB_EVENTS_MAX_MESSAGE_BYTES);
    config.max_frame_size = Some(WEB_EVENTS_MAX_MESSAGE_BYTES);
    // Eager writes so session events are not stuck in the 128 KiB default buffer.
    config.write_buffer_size = 0;
    // Explicit write-buffer cap: tungstenite's default is unbounded
    // (usize::MAX). With eager writes the buffer only grows while the peer is
    // not draining, so one max-size frame is the most backpressure we allow
    // before the connection fails with WriteBufferFull.
    config.max_write_buffer_size = WEB_EVENTS_MAX_MESSAGE_BYTES;
    config.read_buffer_size = 8 * 1024;
    config
}

pub(in crate::server) fn validate_events_websocket_request(
    request: &ParsedHttpRequest,
) -> Result<(), String> {
    if request.method != "GET" {
        return Err("WebSocket upgrade requires GET".to_owned());
    }
    if request.path != "/api/events" {
        return Err("WebSocket path must be /api/events".to_owned());
    }
    if !request.upgrade_websocket || !request.connection_upgrade {
        return Err("missing WebSocket upgrade headers".to_owned());
    }
    if request.sec_websocket_version.as_deref() != Some("13") {
        return Err("unsupported Sec-WebSocket-Version".to_owned());
    }
    let key = request
        .sec_websocket_key
        .as_deref()
        .ok_or_else(|| "missing Sec-WebSocket-Key".to_owned())?;
    if !sec_websocket_key_valid(key) {
        return Err("invalid Sec-WebSocket-Key".to_owned());
    }
    if !web_origin_allowed(request.origin.as_deref(), request.host.as_deref()) {
        return Err("origin not allowed".to_owned());
    }
    Ok(())
}

/// RFC 6455: Sec-WebSocket-Key is a base64-encoded value that decodes to 16 bytes.
pub(in crate::server) fn sec_websocket_key_valid(key: &str) -> bool {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    match BASE64.decode(key) {
        Ok(bytes) => bytes.len() == 16,
        Err(_) => false,
    }
}
