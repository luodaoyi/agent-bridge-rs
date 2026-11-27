use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Write},
    net::TcpStream,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use anyhow::Result;
use tungstenite::{
    Message, WebSocket,
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
};

use crate::{
    protocol::{decode_write_data, validate_session_handle, validate_terminal_size},
    session::SessionHost,
};

use super::{
    RuntimeState, WEB_EVENTS_CLIENT_POLL, WEB_EVENTS_MAX_MESSAGE_BYTES,
    WEB_EVENTS_MAX_REQUEST_ID_BYTES, WEB_EVENTS_MAX_TRACKED_REQUEST_IDS, WEB_HTTP_WRITE_TIMEOUT,
    http::{ParsedHttpRequest, web_origin_allowed, write_http},
    now_millis,
};

pub(super) fn handle_events_websocket(
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
    let key = request
        .sec_websocket_key
        .as_deref()
        .expect("validated websocket key");
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
pub(super) fn web_socket_config() -> WebSocketConfig {
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

fn run_events_websocket(websocket: &mut WebSocket<TcpStream>, state: &RuntimeState) {
    let mut cursors = HashMap::new();
    // Request ids must be unique within this connection; duplicates are
    // rejected before any PTY side effect.
    let mut seen_ids = HashSet::new();
    let mut seen_revision = state.host.revision();
    if !send_web_events(websocket, state, &mut cursors, true) {
        return;
    }

    while !state.stopping.load(Ordering::Acquire) {
        // Service inbound terminal_input / terminal_resize before sleeping so
        // interactive keystrokes never wait behind a multi-second idle timeout.
        match poll_websocket_client(websocket, state, &mut seen_ids) {
            WsClientAction::Continue => {}
            WsClientAction::Close => return,
        }

        let now = now_millis();
        let lease_deadline = state
            .host
            .next_client_lifecycle_deadline_ms()
            .ok()
            .flatten();
        // Sleep until the next host revision *or* the next pure-time lease
        // transition, but never longer than WEB_EVENTS_CLIENT_POLL so client
        // frames stay low-latency. Timeouts without a revision/lease signal
        // never push frames. The event stream itself never refreshes Codex
        // leases (only inbound terminal commands do), so real lease
        // transitions still arrive promptly for display.
        let wait = match lease_deadline {
            Some(deadline) if deadline > now => Duration::from_millis(deadline - now)
                .min(Duration::from_secs(30))
                .max(Duration::from_millis(1))
                .min(WEB_EVENTS_CLIENT_POLL),
            Some(_) => Duration::from_millis(1),
            None => WEB_EVENTS_CLIENT_POLL,
        };
        let current = state.host.wait_revision(seen_revision, wait);
        match poll_websocket_client(websocket, state, &mut seen_ids) {
            WsClientAction::Continue => {}
            WsClientAction::Close => return,
        }
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        let now_after = now_millis();
        let lease_due = lease_deadline.is_some_and(|deadline| now_after >= deadline);
        if current == seen_revision && !lease_due {
            continue;
        }
        if current != seen_revision {
            seen_revision = current;
        }
        if !send_web_events(websocket, state, &mut cursors, false) {
            return;
        }
    }

    let _ = websocket.close(None);
}

/// Plan frames from immutable cursors, send each frame, and commit cursor
/// advances only after that frame is successfully written. Oversize/encode
/// failures never advance cursors and never silently drop committed bytes.
fn send_web_events(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    cursors: &mut HashMap<String, u64>,
    force_reset: bool,
) -> bool {
    let frames =
        match state
            .host
            .plan_web_events(cursors, force_reset, WEB_EVENTS_MAX_MESSAGE_BYTES)
        {
            Ok(frames) => frames,
            Err(error) => {
                eprintln!("agent-bridge server: WebUI events plan failed: {error:#}");
                return true;
            }
        };

    for frame in frames {
        let payload = match serde_json::to_string(&frame.message) {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!("agent-bridge server: WebUI events encode failed: {error}");
                // Do not commit any remaining planned cursors.
                return true;
            }
        };
        if payload.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
            eprintln!(
                "agent-bridge server: WebUI events frame exceeds {} bytes; leaving cursors uncommitted",
                WEB_EVENTS_MAX_MESSAGE_BYTES
            );
            return true;
        }
        if websocket
            .send(Message::text(payload))
            .and_then(|()| websocket.flush())
            .is_err()
        {
            return false;
        }
        for (session, cursor) in frame.cursor_commits {
            cursors.insert(session, cursor);
        }
        for session in frame.cursor_drops {
            cursors.remove(&session);
        }
    }
    true
}

enum WsClientAction {
    Continue,
    Close,
}

fn poll_websocket_client(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    seen_ids: &mut HashSet<String>,
) -> WsClientAction {
    // Drain a bounded number of control/application frames so a noisy client
    // cannot pin this connection thread forever.
    for _ in 0..32 {
        match websocket.read() {
            Ok(Message::Ping(payload)) => {
                if websocket.send(Message::Pong(payload)).is_err() {
                    return WsClientAction::Close;
                }
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => {
                let _ = websocket.close(None);
                return WsClientAction::Close;
            }
            Ok(Message::Text(text)) => {
                if matches!(
                    handle_web_events_client_text(websocket, state, seen_ids, text.as_str()),
                    WsClientAction::Close
                ) {
                    return WsClientAction::Close;
                }
            }
            // Binary frames are not part of the JSON command protocol.
            Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                return WsClientAction::Continue;
            }
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed)
            | Err(tungstenite::Error::Protocol(_)) => {
                return WsClientAction::Close;
            }
            Err(_) => return WsClientAction::Close,
        }
    }
    WsClientAction::Continue
}

/// Client → server command on `/api/events` (JSON text only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WebEventsClientCommand {
    TerminalInput {
        id: String,
        session: String,
        data_base64: String,
    },
    TerminalResize {
        id: String,
        session: String,
        cols: u16,
        rows: u16,
    },
}

/// A parse failure that may still carry enough structure — a legal request id,
/// a recognized command type, and a cleanly extractable session — to correlate
/// the error result with the browser's pending command. Only messages where
/// nothing usable can be identified (invalid JSON, non-object, oversized, or a
/// malformed id) stay id-less with the generic `input_result` type.
#[derive(Debug)]
pub(super) struct WebEventsParseError {
    pub(super) message: String,
    pub(super) id: Option<String>,
    pub(super) session: Option<String>,
    pub(super) result_type: &'static str,
}

impl WebEventsParseError {
    fn generic(message: impl Into<String>) -> Self {
        WebEventsParseError {
            message: message.into(),
            id: None,
            session: None,
            result_type: "input_result",
        }
    }

    fn with_context(
        message: impl Into<String>,
        id: Option<String>,
        session: Option<String>,
        result_type: &'static str,
    ) -> Self {
        WebEventsParseError {
            message: message.into(),
            id,
            session,
            result_type,
        }
    }
}

/// Parse a single WebUI client command without panicking on junk.
/// Unknown types yield `Ok(None)` (ignored). Malformed known types yield `Err`
/// carrying the recognized request id and result type when available.
pub(super) fn parse_web_events_client_command(
    text: &str,
) -> Result<Option<WebEventsClientCommand>, WebEventsParseError> {
    if text.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
        return Err(WebEventsParseError::generic("message exceeds size limit"));
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| WebEventsParseError::generic(format!("invalid JSON: {error}")))?;
    let Some(object) = value.as_object() else {
        return Err(WebEventsParseError::generic(
            "message must be a JSON object",
        ));
    };
    let message_type = object
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // Best-effort extraction before the field checks below, so a malformed
    // known-shape command can still be settled by its request id and type.
    let id = match object.get("id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => {
            if value.len() > WEB_EVENTS_MAX_REQUEST_ID_BYTES {
                return Err(WebEventsParseError::generic("request id is too long"));
            }
            Some(value.clone())
        }
        Some(_) => return Err(WebEventsParseError::generic("request id must be a string")),
    };
    let session = object
        .get("session")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let result_type = match message_type {
        "terminal_input" => "input_result",
        "terminal_resize" => "resize_result",
        // Unknown / push-only types (e.g. future clients): ignore safely.
        _ => return Ok(None),
    };
    // Errors past this point keep the recognized id/session/type so the
    // browser can release its pending entry and see a correlated failure.
    // The closure owns its own clones so `id` below can still be moved into
    // the command variants.
    let context_error = {
        let id = id.clone();
        let session = session.clone();
        move |message: String| {
            WebEventsParseError::with_context(message, id.clone(), session.clone(), result_type)
        }
    };

    match message_type {
        "terminal_input" => {
            let Some(id) = id else {
                return Err(context_error("request id is required".to_owned()));
            };
            if id.is_empty() {
                return Err(context_error("request id is required".to_owned()));
            }
            let session = object
                .get("session")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            validate_session_handle(&session).map_err(|err| context_error(format!("{err:#}")))?;
            let data_base64 = object
                .get("data_base64")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            if data_base64.is_empty() {
                return Err(context_error("data_base64 is required".to_owned()));
            }
            Ok(Some(WebEventsClientCommand::TerminalInput {
                id,
                session,
                data_base64,
            }))
        }
        "terminal_resize" => {
            let Some(id) = id else {
                return Err(context_error("request id is required".to_owned()));
            };
            if id.is_empty() {
                return Err(context_error("request id is required".to_owned()));
            }
            let session = object
                .get("session")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            validate_session_handle(&session).map_err(|err| context_error(format!("{err:#}")))?;
            let cols = object
                .get("cols")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| context_error("cols is required".to_owned()))?;
            let rows = object
                .get("rows")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| context_error("rows is required".to_owned()))?;
            let cols =
                u16::try_from(cols).map_err(|_| context_error("cols out of range".to_owned()))?;
            let rows =
                u16::try_from(rows).map_err(|_| context_error("rows out of range".to_owned()))?;
            Ok(Some(WebEventsClientCommand::TerminalResize {
                id,
                session,
                cols,
                rows,
            }))
        }
        _ => Ok(None),
    }
}

/// Apply a parsed command through SessionHost, reusing write_raw / resize.
pub(super) fn apply_web_events_client_command(
    host: &SessionHost,
    command: &WebEventsClientCommand,
) -> Result<(), String> {
    // An inbound command proves the WebUI is attached right now. Refresh before
    // touching the PTY so a provisional orphan claim is canceled before input
    // or resize is accepted.
    host.touch_web_clients()
        .map_err(|error| format!("{error:#}"))?;
    match command {
        WebEventsClientCommand::TerminalInput {
            session,
            data_base64,
            ..
        } => {
            let data = decode_write_data(data_base64).map_err(|error| format!("{error:#}"))?;
            host.write_raw(session, data)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        }
        WebEventsClientCommand::TerminalResize {
            session,
            cols,
            rows,
            ..
        } => {
            validate_terminal_size(*cols, *rows).map_err(|error| format!("{error:#}"))?;
            host.resize(session, *cols, *rows)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        }
    }
}

fn web_events_result_type(command: &WebEventsClientCommand) -> &'static str {
    match command {
        WebEventsClientCommand::TerminalInput { .. } => "input_result",
        WebEventsClientCommand::TerminalResize { .. } => "resize_result",
    }
}

fn web_events_command_session(command: &WebEventsClientCommand) -> &str {
    match command {
        WebEventsClientCommand::TerminalInput { session, .. }
        | WebEventsClientCommand::TerminalResize { session, .. } => session.as_str(),
    }
}

fn web_events_command_id(command: &WebEventsClientCommand) -> &str {
    match command {
        WebEventsClientCommand::TerminalInput { id, .. }
        | WebEventsClientCommand::TerminalResize { id, .. } => id.as_str(),
    }
}

/// The single result a client command resolves to: applied, rejected by apply
/// validation, or rejected as a duplicate request id. Exactly one result is
/// always sent back so the caller can match it to its pending command.
pub(super) struct WebEventsCommandOutcome {
    pub(super) result_type: &'static str,
    pub(super) id: Option<String>,
    session: Option<String>,
    pub(super) ok: bool,
    pub(super) error: Option<String>,
    pub(super) reconnect: bool,
}

pub(super) fn plan_web_events_command_outcome(
    host: &SessionHost,
    command: &WebEventsClientCommand,
    seen_ids: &mut HashSet<String>,
) -> WebEventsCommandOutcome {
    let result_type = web_events_result_type(command);
    let id = web_events_command_id(command).to_owned();
    let session = Some(web_events_command_session(command).to_owned());
    // Strict per-connection uniqueness with a hard memory bound. Eviction
    // would let a previously-applied non-idempotent input replay, so a full
    // window rejects one new command and explicitly rotates the connection.
    if seen_ids.contains(&id) {
        return WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some("duplicate request id".to_owned()),
            reconnect: false,
        };
    }
    if seen_ids.len() >= WEB_EVENTS_MAX_TRACKED_REQUEST_IDS {
        return WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some("request id window exhausted; reconnect required".to_owned()),
            reconnect: true,
        };
    }
    seen_ids.insert(id.clone());
    match apply_web_events_client_command(host, command) {
        Ok(()) => WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: true,
            error: None,
            reconnect: false,
        },
        Err(error) => WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some(error),
            reconnect: false,
        },
    }
}

/// Build a result envelope that never collides with `{ type: "sessions" }` push frames.
pub(super) fn build_web_events_command_result(
    result_type: &str,
    id: Option<&str>,
    session: Option<&str>,
    ok: bool,
    error: Option<&str>,
    reconnect: bool,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "type".to_owned(),
        serde_json::Value::String(result_type.to_owned()),
    );
    map.insert("ok".to_owned(), serde_json::Value::Bool(ok));
    if let Some(id) = id {
        map.insert("id".to_owned(), serde_json::Value::String(id.to_owned()));
    }
    if let Some(session) = session {
        map.insert(
            "session".to_owned(),
            serde_json::Value::String(session.to_owned()),
        );
    }
    if let Some(error) = error {
        map.insert(
            "error".to_owned(),
            serde_json::Value::String(error.to_owned()),
        );
    }
    if reconnect {
        map.insert("reconnect".to_owned(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(map).to_string()
}

fn send_web_events_command_result(
    websocket: &mut WebSocket<TcpStream>,
    result_type: &str,
    id: Option<&str>,
    session: Option<&str>,
    ok: bool,
    error: Option<&str>,
    reconnect: bool,
) -> Result<(), ()> {
    let payload = build_web_events_command_result(result_type, id, session, ok, error, reconnect);
    if payload.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
        return Err(());
    }
    websocket
        .send(Message::text(payload))
        .and_then(|()| websocket.flush())
        .map_err(|_| ())
}

fn handle_web_events_client_text(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    seen_ids: &mut HashSet<String>,
    text: &str,
) -> WsClientAction {
    match parse_web_events_client_command(text) {
        Ok(None) => WsClientAction::Continue,
        Ok(Some(command)) => {
            // terminal_input and terminal_resize always resolve to exactly one
            // result frame (input acks included) keyed by the connection-unique
            // request id, so the browser can settle its pending command.
            let outcome = plan_web_events_command_outcome(&state.host, &command, seen_ids);
            let sent = send_web_events_command_result(
                websocket,
                outcome.result_type,
                outcome.id.as_deref(),
                outcome.session.as_deref(),
                outcome.ok,
                outcome.error.as_deref(),
                outcome.reconnect,
            );
            if sent.is_err() {
                return WsClientAction::Close;
            }
            if outcome.reconnect {
                let _ = websocket.close(None);
                WsClientAction::Close
            } else {
                WsClientAction::Continue
            }
        }
        Err(error) => {
            // Malformed known-shape or generic junk that failed parse: never
            // touch PTY. If the parse preserved a recognized id/type/session,
            // send a correlated failure so the browser can settle (and release
            // the admission bytes of) its pending command; fully-unidentifiable
            // junk falls back to a generic input_result the browser ignores.
            let sent = send_web_events_command_result(
                websocket,
                error.result_type,
                error.id.as_deref(),
                error.session.as_deref(),
                false,
                Some(&error.message),
                false,
            );
            if sent.is_err() {
                WsClientAction::Close
            } else {
                WsClientAction::Continue
            }
        }
    }
}

pub(super) fn validate_events_websocket_request(request: &ParsedHttpRequest) -> Result<(), String> {
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
pub(super) fn sec_websocket_key_valid(key: &str) -> bool {
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
